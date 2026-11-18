# A mind as one Durable Object

`crates/cloudflare` runs mensor on Cloudflare Workers. Each mind is one
Durable Object holding its history, its one hand, `local`, and the hand's
files in the object's SQLite. Nothing dials in: a prompt wakes the object,
and it sleeps when the mind is idle. Mind and hand pass
[the protocol](protocol.md)'s messages in memory.

![A request reaches a Worker, which finds the Durable Object named after the mind. Inside, the mind and its hand pass calls in memory, and the history, the hand's memory and its files are tables in the object's SQLite.](images/cloudflare.svg)

## Run it

Needs Node (the version in `.node-version`) and `make tools` for
worker-build. `CLOUDFLARE_INCLUDE_PROCESS_ENV` hands the worker every
variable of the shell that starts it, so start from an empty one:

```bash
cd crates/cloudflare
env -i PATH="$PATH" HOME="$HOME" CLOUDFLARE_INCLUDE_PROCESS_ENV=true \
  ANTHROPIC_API_KEY="$(cat path/to/anthropic.key)" \
  npx wrangler@4.160.1 dev
```

```bash
api=127.0.0.1:8787/v1/minds
curl -s -X POST $api/m/prompt -H 'content-type: application/json' \
  -d '{"key":"1","text":"Fetch example.com and save its title to title.txt."}'
curl -sN $api/m/events
```

To deploy:

```bash
npx wrangler@4.160.1 secret put ANTHROPIC_API_KEY
npx wrangler@4.160.1 secret put MENSOR_TOKEN
npx wrangler@4.160.1 deploy
```

| Variable | Default | |
| --- | --- | --- |
| `ANTHROPIC_API_KEY` or `ANTHROPIC_AUTH_TOKEN`, `OPENAI_API_KEY` | none | The model's credential, a secret. `MENSOR_API` picks the API as for [a host](../README.md#configuration). |
| `MENSOR_TOKEN` | none | The bearer token every request needs, a secret. Without it only a loopback `Host` is answered, so a deployed worker refuses every request. Tokens for one mind are not taken. |
| `MENSOR_MODEL` | `claude-opus-5-5` | The model every mind asks. |
| `MENSOR_EFFORT`, `MENSOR_SYSTEM` | none | Its effort and system prompt. |
| `MENSOR_FALLBACKS` | on | `off` turns off the API's fallback model for a request Claude declines. |
| `MENSOR_COMPACT_AT` | `150000` | Tokens before the mind summarises; `0` never does. |
| `ANTHROPIC_BASE_URL`, `OPENAI_BASE_URL` | the API's | Another address for the API. |
| `MENSOR_RECOVERY_MS` | `30000` | How soon an object stopped mid-work is woken. |

## The API

A host's `prompt`, `interrupt`, `GET /v1/minds/{m}`, `history` and
`events`, one mind at a time. A prompt's `key` makes retries safe. The
first prompt to a name starts its mind, and `GET` is 404 until then. There
is no `/v1/attach`, `release`, `PUT`, `end` or `GET /v1/minds`: the one hand
is inside, every mind has the worker's settings, and each object knows only
itself. A mind is offered none of its own tools: its hand cannot clone, and
it has no parent or children.

| Capability | Tools | Backed by |
| --- | --- | --- |
| `files` | `read`, `write`, `edit`, `list` | A row per file in the `files` table |
| `fetch` | `fetch` | The platform's `fetch`: the first 256 KiB of a body, within a minute |

## How an object works

- **The alarm is its only worker.** A prompt is stored, sets the alarm for
  now, and is answered. The alarm then asks the model or runs a call, one
  at a time, until the mind is idle. An interrupt stops it waiting at once.
- **A wake has a budget:** 32 pieces of work or eight minutes, then it sets
  the alarm again. A model request gets four minutes and four retries.
- **A stop is recovered.** Work sets the alarm `MENSOR_RECOVERY_MS` ahead,
  so an object stopped by a deploy, crash or eviction wakes and goes on:

| It stopped while… | On waking |
| --- | --- |
| asking the model | It asks again. |
| a `files` call ran | The call and its outcome were stored, or neither and the model is asked again. Unknown if a wake ended between the reply and the call. |
| a `fetch` call ran | It may have run, so its outcome is unknown and it is not made again. |

A row holds at most 2 MB, so one history item or file is smaller. Recovery
is tested on workerd, not on Cloudflare's network.
