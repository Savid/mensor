# A mind as one Durable Object

`crates/cloudflare` runs mensor on Cloudflare Workers. Each mind is one
Durable Object, which holds the mind's history, its one hand, `local`, and
the hand's files in the object's own SQLite database. Nothing dials in and
nothing is hosted: a prompt wakes the object, and it sleeps when the mind is
idle.

```mermaid
flowchart LR
  client["client"] -- "POST /v1/minds/a1/prompt" --> worker["Worker"]
  worker -- "by name" --> object
  subgraph object["Durable Object a1"]
    mind["mind"] -- "call, in memory" --> hand["hand local"]
    hand --> db[("SQLite: history, hand, files")]
    mind --> db
  end
  mind -- "fetch" --> model["the model"]
```

The mind and the hand are the ones a server and a remote hand run. They
pass the protocol's messages in memory, through `mensor_object::Object`, so
every rule in [protocol.md](protocol.md) holds here too.

## Run it

Needs Node (`.node-version` names the one CI runs), and `make tools` at the
repository's root for worker-build.

```bash
cd crates/cloudflare
env -i PATH="$PATH" HOME="$HOME" CLOUDFLARE_INCLUDE_PROCESS_ENV=true \
  ANTHROPIC_API_KEY="$(cat path/to/anthropic.key)" \
  npx wrangler dev
```

`CLOUDFLARE_INCLUDE_PROCESS_ENV` hands the worker every variable of the shell
that starts it, which is why the command starts from an empty one. With no
`MENSOR_TOKEN`, the worker serves only requests addressed to this machine.

```bash
api=127.0.0.1:8787/v1/minds
curl -s -X POST $api/a1/prompt -H 'content-type: application/json' \
  -d '{"key":"1","text":"Fetch example.com and save its title to title.txt."}'
curl -sN $api/a1/events
```

To deploy, set the secrets and publish:

```bash
npx wrangler secret put ANTHROPIC_API_KEY
npx wrangler secret put MENSOR_TOKEN
npx wrangler deploy
```

| Variable | Meaning |
| --- | --- |
| `ANTHROPIC_API_KEY` or `ANTHROPIC_AUTH_TOKEN` | The credential of a model behind Claude's Messages API. A secret. |
| `OPENAI_API_KEY` | The credential of a model behind Chat Completions. A secret. |
| `MENSOR_API` | `messages` or `chat`, when both credentials are set. |
| `MENSOR_TOKEN` | The bearer token every request must carry. A secret. Without it, only this machine is served. |
| `MENSOR_MODEL` | The model every mind asks. `claude-opus-5-5` unless set. |
| `MENSOR_EFFORT` | How hard the model works, in its API's words: for Claude `low`, `medium`, `high`, `xhigh` or `max`. |
| `MENSOR_SYSTEM` | The system prompt every mind gives its model. |
| `MENSOR_FALLBACKS` | `off` where the Messages API has no fallback model. |
| `MENSOR_COMPACT_AT` | The tokens a request may hold before the mind sums the conversation up. `150000` unless set, and never when `0`. |
| `ANTHROPIC_BASE_URL`, `OPENAI_BASE_URL` | Another address for the API. |
| `MENSOR_RECOVERY_MS` | How soon an object stopped mid-work is woken. `30000` unless set. |

## The API

It is the API of `mensor serve`, for one mind at a time.

| Request | Answer |
| --- | --- |
| `POST /v1/minds/{name}/prompt` | Stores `{"text", "key"?}`. 202 `accepted`, 200 `existing` for a key already stored, 409 for a key stored with other text. The first prompt to a name starts its mind, whose name has no `.` and at most 55 bytes. |
| `POST /v1/minds/{name}/interrupt` | Stops the turn, and answers with the mind as the next row gives it. |
| `GET /v1/minds/{name}` | `{"name", "origin", "state", "hands", "waiting", "length"}`. 404 until the mind is prompted. |
| `GET /v1/minds/{name}/history?after=` | The items from a cursor, 512 at the most, and the next cursor. |
| `GET /v1/minds/{name}/events?after=` | The same as server-sent events, each new item as it is stored. Between them come `writing` events: the reply the model is writing, piece by piece. |

Four things differ from the server. There is no `/v1/attach`: the hand is
already there, so `hands` is always `["local"]` and `waiting` empty. There
is no `/release`: the one hand is never let go. There is no `GET
/v1/minds`: each object knows only itself. And a mind is born by its first
prompt, not by a hand attaching.

The hand has no `clone` capability, so its mind is offered no tools of its
own, and forks nothing.

## What the hand can do

| Capability | Operations | Backed by |
| --- | --- | --- |
| `files` | `read`, `write`, `edit`, `list` | Rows of the object's `files` table, one per file, keyed by path |
| `fetch` | `fetch` | The platform's `fetch`. It keeps the first 256 KiB of a body, and gives up after a minute |

`files` is the capability a hand on a real machine has, over another
backing, so the model sees the same four tools. A hand here runs no
commands: an object has no machine.

## How an object works

The object keeps three tables: `items` (the history, one row per item),
`hand` (what the hand remembers, one row) and `files`.

**The alarm is the object's only worker.** A prompt is stored, sets the
alarm for now and is answered. The alarm then takes one piece of work at a
time, asks the model or runs a call, and stores what came of it, until the
mind is idle. A prompt that arrives while it works is stored at once and
joins the turn at the next model call.

**An interrupt frees the object at once.** The alarm stops waiting for the
model or the page it had asked for, and the mind is idle when the request
that interrupted it is answered. An answer that arrives later is dropped.

**One wake has a budget.** After 32 pieces of work, or eight minutes, a wake
sets the alarm for now and ends, so that none outgrows what the platform
allows one alarm.

**Writes with no wait between them are one commit.** The platform stores
together every write made between two awaits. A `files` call never waits,
so the model's reply, the call it asked for, the file it changed and the
call's outcome are stored as one, or not at all.

## What a stop does

An object can stop at any instant: a deploy, a crash, an eviction. When its
work starts, it sets its alarm to `MENSOR_RECOVERY_MS` from then, and clears
it when the work ends. An object that stops mid-work is woken by that alarm
and opens what it stored.

| It stopped while… | When it wakes |
| --- | --- |
| the model was being asked | The model is asked again. Nothing of the lost request was stored. |
| a `files` call ran | This cannot be seen: the call and its outcome are one commit. Either both are stored, or the model is asked again. |
| a `fetch` call ran | The call may have run, so it is not made again. Its outcome is recorded as an error that says it is unknown, and the model decides what to do. |
| nothing was happening | Nothing is stored, and no model is asked. It is the same hand, with the same `boot`. |

`tests/workerd.rs` kills the runtime in the first and third of these and
checks what the restarted object does with nobody prompting it.

## Limits

- One row holds at most 8 MB, the platform's limit, so one item of history
  and one file are each smaller than that.
- An alarm runs for at most fifteen minutes. One request to the model is
  given four.
- An object that is awake holds the end of its history: the items from
  its last summary, or all of it until there is one. The rest is read from
  its database when it is asked for.
- Not built: clones and forks, a remote hand attaching to an object's mind,
  listing minds, and deleting one.
- It has run on workerd under `wrangler dev` and on Cloudflare's network,
  with a model behind Chat Completions. A deploy there did not stop an
  object in the middle of a model request: the reply ended under the old
  version, so the recovery alarm has been tested only on workerd.

## Tests

```bash
make test-cloudflare
```

This builds the worker and runs `crates/cloudflare/tests/workerd.rs`: the
worker on workerd under `wrangler dev`, with a scripted model beside it and
an empty environment. The rules themselves are tested without a runtime, in
`crates/object/tests/object.rs`.
