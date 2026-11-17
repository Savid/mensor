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
own, or each in a container as [examples/docker](examples/docker) does. The
first hand to name a mind starts it:

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
any language: [examples/own-hand](examples/own-hand) is one in Python, and
[examples/opencode](examples/opencode) makes opencode one.

Commands start without the host's token, the model's credentials or the
store's URL in their environment, and on Linux every mensor process closes
its environment and memory to its user's other processes as it starts.
What a command leaves running outlives its call but not its hand: when the
hand ends, or a signal it handles stops it, its commands' process groups
are killed. That leaves open:

- a process of the same user already running, which can catch a binary as
  it starts;
- jobs left behind by a hand killed with SIGKILL;
- root with `CAP_SYS_PTRACE`;
- whatever holds the variables around mensor: a shell script, an init such
  as tini, a `docker exec` shell;
- macOS, where a process can read its user's other processes' environments.

A hand is not a sandbox: a command can reach whatever the hand's process
can, so confining it is up to whoever runs it. `mensor run` is for one
person, on a machine they trust the model with: its commands run beside the
process that holds the model's credential.

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
and `tell` and `wait` while a hand can clone or the mind has kin. A `tell`
with no parent or child, or a `wait` with no children, does not run, and
says why. Forking needs a hand that can clone, so a fork leaves a mind's
tools as they were, and its children start with the same. `--mind-tools`,
a comma list, picks which of them a host offers at all
(`fork,clone_hand,release_hand,tell,wait` by default): `--mind-tools fork`
lets a mind fork, and neither clone a hand nor let one go, and
`--mind-tools ''` offers none.

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
  `--max-forks unlimited` splits nothing: whether a fork is made is then for
  the cloning hand to say, by its `clone` succeeding or failing.
- A child is made with its parent's model, effort and system prompt.
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

- `tell` reaches the other mind as a prompt marked with the sender's name,
  "(From m:)" or "(From m.d3342iuj:)", the same for every mind that shares
  it. A message sent again after a restart is stored once.
- `wait` returns what each child said last, or that it was stopped, its
  model failed, or it ended. Only a parent waits, so two minds never wait
  on each other. An interrupt ends a wait.
- A message to a mind that has ended does not run, and the model is told
  so.
- A waiting parent needs no hand. It may rest, or its host may restart: it
  watches its children again when it wakes.

## API

| Request | Does |
| --- | --- |
| `GET /v1/minds` | Lists every mind, each without its system prompt. |
| `GET /v1/minds/{m}` | `{name, origin, state, hands, waiting, watching, length}`, and the settings it was made with: what it is doing, its connected hands, and the hands and children its turn waits for. `state` is `idle`, `thinking`, `working`, `waiting` (it can go on only when a hand connects: one named in `waiting`, or its first if it has none) or `ended`. `origin` is `root`, `fork` with `parent` and `inherited`, or `from` with `from`. |
| `PUT /v1/minds/{m}` | Makes `m` with settings of its own, and maybe from another mind's history. See [Making a mind](#making-a-mind). |
| `POST /v1/minds/{m}/prompt` | Stores `{"text", "key"?}`. 202 if new, 200 if the key is already stored, 409 if the key holds other text or the mind has ended. |
| `POST /v1/minds/{m}/interrupt` | Stops the turn. |
| `POST /v1/minds/{m}/release` | Lets a hand go: `{"hand": "a1"}`. |
| `POST /v1/minds/{m}/end` | Ends `m`: its turn stops, every hand is let go (4004), and it asks its model nothing more and takes no prompt it does not hold already. Its history stays, for its children and for minds made from it. |
| `GET /v1/minds/{m}/history?after=` | Up to 512 items from index `after`, and the next cursor. |
| `GET /v1/minds/{m}/events?after=` | The same as server-sent events, live, with `writing` events that carry each reply as the model writes it. An event's id is its item's index: resume with `after` set to the last id plus one. |
| `GET /v1/attach` | The WebSocket a hand dials. See [docs/protocol.md](docs/protocol.md). |

With `MENSOR_TOKEN` set, every request needs `Authorization: Bearer <token>`,
and a hand sends the one in its own `MENSOR_TOKEN`. The host's token reaches
every mind. `mensor token --mind m` prints one for `m` alone, to give `m`'s
hands instead:

```bash
hand_token="$(mensor token --mind m)"   # MENSOR_TOKEN holds the host's
MENSOR_TOKEN="$hand_token" mensor hand --mind m --hand a1 --url wss://mensor.example/v1/attach
```

A token for `m`:

- reaches `m` and its children, all of them: it can prompt and interrupt
  them, release their hands, read their histories, and say hello as any of
  their hands, taking its place;
- gets 403 for any other mind, and a hello for one is closed with 4006.
  `GET /v1/minds` lists only `m`'s;
- is kept nowhere by the host: a new `MENSOR_TOKEN` ends them all;
- separates only hands that run as another user or on another machine than
  the host: a hand of the host's own user can read its store.

Carry it as any secret, and never in a file in a hand's directory, where the
model reads.

Without `MENSOR_TOKEN`, the host trusts every process on its machine. It
listens only on loopback, and answers 403 when `Host` is not `localhost` or
a loopback address, or when a browser's `Origin` is not one either. That
stops browsers only: a web page can't reach the host by DNS rebinding, or
open a WebSocket to it as a hand.

`--listen` sets where the host listens (`127.0.0.1:7878`), and a hand's
`--url` where it dials. The host speaks plain HTTP: off loopback, put TLS in
front of it and have hands dial `wss://`.

### Making a mind

A hand that says hello to a root no one made starts it, with the host's
settings. `PUT /v1/minds/{m}` makes it first, with its own, which are fixed
from then on:

| Field | Is |
| --- | --- |
| `model` | The model it asks: `--model`, or one of `--models`. |
| `effort` | How hard that model works: for the Messages API `low`, `medium`, `high`, `xhigh` or `max`, and for the others any the server takes. |
| `system` | Its system prompt, in place of `--system-file`'s. |
| `from` | `{"mind", "at", "hand"}`: start from the first `at` items of `mind`'s history, keeping its hand `hand`. |

- A setting left out is the host's, or with `from`, that mind's. A fork's
  children are made with their parent's.
- With `from`, the mind shares those items, as a fork's child does, and
  none stored after them. To start just after an item, `at` is its index
  plus one. `mind` may have ended, so long as `at` falls before its end.
- It keeps `hand`, which says hello to it as that hand again: a hand that
  can stand at `at`, such as a VM restored from a snapshot taken then. A
  new process's hello makes a call that hand had not answered unknown; the
  same process's settles it by what it has read, runs and holds, as any
  hello does. The rest of the step then runs on it. Its calls are numbered
  above every one `mind` had made, so a hand restored from later than `at`
  reads them as new.
- Every other hand stays behind, and so does a clone in flight at `at`,
  whose outcome is unknown. The mind is no kin of `mind`, and forks as one
  that starts from nothing.
- 201 when made, 200 when made the same way before, 409 when made another
  way, 404 when `from` names no mind. 400 for a name with a `.` or of more
  than 55 bytes, a model the host does not offer, an effort its model's API
  does not take, or a `from` it cannot be made from. With a token for one
  mind, `from` must name one it reaches, or it is 403.
- `PUT` comes before the hand's hello: once a hello has started the mind
  with the host's settings, a `PUT` with others is 409.

`POST /v1/minds/{m}/end` releases every hand of the mind, as
`POST /release` does each: `mensor hand` deletes a released clone's copy,
so ending a fork's child deletes its directory. A mind made `from` an
ended one needs a hand that can stand where it starts.

## Models

| Credential | API | Address |
| --- | --- | --- |
| `ANTHROPIC_API_KEY` or `ANTHROPIC_AUTH_TOKEN` | Claude's Messages API | `ANTHROPIC_BASE_URL`, or Anthropic's |
| `OPENAI_API_KEY` | Chat Completions (OpenAI, OpenRouter and most others), or with `MENSOR_API=responses` OpenAI's Responses API | `OPENAI_BASE_URL`, or OpenAI's |

With both set it is the Messages API, unless `MENSOR_API` is `chat` or
`responses`. `--model` picks the model (`claude-opus-5-5` by default, so
name another for the others), and `--effort` how hard it works. `--models`,
a comma list, names more a mind may be made with, through the same API and
credential. A request Claude declines may be answered by a fallback model,
unless `--no-fallbacks` is set.

- `ANTHROPIC_AUTH_TOKEN` goes on `Authorization` as a plain bearer, as a
  gateway in front of the API takes it. A Claude.ai OAuth token is not one.
- The Messages API is asked to think on every request, as hard as the
  effort says, and to summarise its reasoning, which otherwise streams as
  empty text. Its models must be Claude 4.6 or later: older ones refuse
  adaptive thinking.
- A step's `usage` counts its tokens by price: `input` (neither read from
  nor written to the cache), `cacheRead`, `cacheWritten` and `output`. It
  counts only the attempt that answered, whose `model` the step names: with
  fallbacks, an attempt the first model declined is billed but not
  counted.
- Each step is stored with a fingerprint of the model, system prompt and
  tools it was asked under, and its reasoning goes back to the model only
  under the same: under any other the API would refuse it, or drop it and
  every block after it. What the history says is written the same for every
  mind that shares it, so a fork's child on its parent's setup starts with
  its parent's last request, reasoning and all, and reads its cache.
- The Responses API stores nothing (`store: false`): each request carries
  the conversation, and a step's output items go back as they came, its
  reasoning encrypted and its messages with their phase. Items the API
  refuses, such as another organization's reasoning, are left out once and
  the request sent again. With an effort it is also asked for a summary of
  the reasoning, which OpenAI may give only to a verified organization.
- `--max-tokens` (16,000, at least 16) caps one reply: the Messages API's
  `max_tokens`, Chat Completions' `max_completion_tokens`, which OpenAI
  and OpenRouter both honour, and the Responses API's `max_output_tokens`.
  `none` sends no cap, for a server that refuses one; the Messages API
  needs one. A reply cut off while it asks for a tool is asked for again,
  with that tool not run; one cut off in its text ends the turn. Every API
  counts reasoning in it, so a model at a high effort can spend it all
  thinking.
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

- A host keeps every history in one store: a SQLite file (`--store`), a
  Postgres database (`MENSOR_POSTGRES`), or, with neither, memory that ends
  with the process. Both set is an error. A child shares its parent's rows
  instead of copying them.
- A store serves one host at a time. Postgres enforces it: a second host is
  refused while the first holds the database.
- A mind is loaded when something concerns it, and unloaded a minute after
  it has no hand, no follower and no model call. A host that starts wakes
  only the minds with work that needs no hand.

### Postgres

`mensor` and `mensor-mind` reach Postgres when built with the `postgres`
feature, which makes each 0.9 MB larger. `MENSOR_POSTGRES` names the
database by a URL, and only it does, never a flag: the URL holds the
password.

```bash
cargo build --release --locked -p mensor --features postgres
export MENSOR_POSTGRES="postgres://mensor:$(cat path/to/db.password)@db.example/mensor"
target/release/mensor serve
```

- `sslmode=disable` is plain. Every other `sslmode`, `prefer` and `require`
  included, is TLS 1.3 checked as libpq's `verify-full` checks it: the
  server's certificate against the system's roots, or those in the file
  `sslrootcert` names (`system` names the system's), and its name against
  the host's. A Unix socket is reached plain, as libpq reaches it.
  `channel_binding=require` is refused: the checked certificate already
  proves the server.
- The host holds its schema with an advisory lock, so a second host is
  refused. A host whose machine vanishes keeps the lock for about a minute,
  until the server gives up on its connection. A host that reconnects takes
  the lock again before it writes; if another host took the schema
  meanwhile, it writes nothing more and must start again.
- The URL must reach Postgres itself, or a pooler in session mode: in
  transaction mode the lock holds nothing.
- The database is UTF8, or it is refused. The tables go in an empty schema,
  and one that holds other tables is refused. To share a database, give
  mensor a schema of its own: `CREATE SCHEMA mensor`, and
  `?options=-c%20search_path%3Dmensor` in the URL.
- A commit waits for the disk (`synchronous_commit = on`), whatever the
  database's default.
- An item is kept as the JSON it was written as, in a `json` column, so SQL
  reads it: `SELECT item ->> 'text' FROM items WHERE kind = 'prompt'`.
- A connection gives up on a server gone quiet within about a minute, and
  is made again. What found it lost runs again, unless it may already have
  been stored.

## Binaries

`make build` makes three, so that a machine carries only the half it runs:

| Binary | Runs | Size |
| --- | --- | --- |
| `mensor` | `serve`, `hand`, `run` and `token` | 5.4 MB |
| `mensor-mind` | what `serve` does, with its flags | 4.9 MB |
| `mensor-hand` | what `hand` does, with its flags | 2.5 MB |

Sizes are for x86-64 Linux, stripped, without the `postgres` feature. Each
logs to standard error at `info`, or at the level `MENSOR_LOG` names.

## Cloudflare

A mind can also run as one Durable Object that holds its history, one hand
(`local`) and the hand's files, behind the same API. It cannot fork. See
[docs/cloudflare.md](docs/cloudflare.md).

## Layout

![What each runtime is built from: the hosts on top, and under them the pure crates, the same ones wherever they are needed.](docs/images/crates.svg)

| Crate | Holds |
| --- | --- |
| `mensor-protocol` | The messages between a mind and a hand, and how a connection is settled. Pure. |
| `mensor-mind` | The history, the turn loop, the tools and forks, as a state machine, and the store trait with the tests every store passes. Pure. |
| `mensor-hand` | A hand's memory, the capability trait, and `files` over any byte store. Pure. |
| `mensor-model` | Requests and answers for the Messages API, Chat Completions and the Responses API, with retries. Pure. |
| `mensor-api` | The HTTP API's bodies, which both hosts serve. Pure. |
| `mensor-object` | A mind and its one hand as one object over a storage trait. Pure. |
| `mensor-mind-host` | The server: a task per mind, the attach endpoint, the API, the model client. |
| `mensor-hand-host` | A hand on a machine: the driver, the WebSocket dial, `commands`, `files` on disk, `clone`. |
| `mensor-cloudflare` | The Cloudflare worker. |
| `mensor-store-sqlite` | The SQLite store. |
| `mensor-store-postgres` | The Postgres store, behind `mensor`'s `postgres` feature. |
| `mensor` | The three binaries, the in-memory dial, and the end-to-end tests. |

The pure crates do no I/O and read no clock, so their tests drive them
directly, and the Cloudflare worker reuses them unchanged.

## Not built yet

- A `wait` that times out.
- The Responses API.
- Deleting a mind, beyond ending it.
- Clones made outside the hand, such as VM snapshots.
- Hands a host gives every mind, such as one that searches the web.
- `fetch` for `mensor hand`.
- Tokens that expire, or that can be taken back one at a time.
- Several hosts over one Postgres database, each with minds of its own.
- A fork's children with models of their own.
- On Cloudflare: forks, remote hands, tokens for one mind, making or ending
  a mind, and listing minds.

MCP is not planned.

## Docs

- [docs/protocol.md](docs/protocol.md): what a mind and its hands say to each other.
- [docs/cloudflare.md](docs/cloudflare.md): a mind as one Durable Object.
- [examples/](examples): a hand of your own, hands in containers, and opencode as a hand.
- [AGENTS.md](AGENTS.md): commands, layout and rules for working here.

## License

[MIT](LICENSE)
