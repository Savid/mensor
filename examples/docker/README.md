# Hands in containers

A host, and hands for two minds, each in a container: `a1` and `b2` for
`m`, `c3` for `n`. They share the repo's image, which has `git`, `ripgrep`,
`python3`, `jq` and `curl` for the model's commands: add what your work
needs to the [`Dockerfile`](../../Dockerfile).

Only the host holds the model's key and the host's token. Each mind's hands
hold a token for that mind, which reaches it and its children: see
[docs/security.md](../../docs/security.md#tokens). The model's commands
can't read it from the hand's process, but a `docker exec` shell has it.

```bash
docker build -t mensor-example .
# or: docker pull ghcr.io/savid/mensor:0.1.6 && docker tag ghcr.io/savid/mensor:0.1.6 mensor-example
export COMPOSE_FILE=examples/docker/compose.yaml
export MENSOR_TOKEN="$(openssl rand -hex 32)"   # the host's: it reaches every mind
export MENSOR_M_TOKEN="$(docker run --rm -e MENSOR_TOKEN mensor-example token --mind m)"
export MENSOR_N_TOKEN="$(docker run --rm -e MENSOR_TOKEN mensor-example token --mind n)"
export ANTHROPIC_API_KEY="$(cat path/to/anthropic.key)"
docker compose up -d
```

For Chat Completions, set `OPENAI_API_KEY` and `OPENAI_BASE_URL` instead
of `ANTHROPIC_API_KEY`, and `MENSOR_MODEL`, which Compose passes to the
host as `--model`: for OpenRouter, `https://openrouter.ai/api/v1` and a
model such as `anthropic/claude-haiku-5.5`.

Talk to it with a token, the host's or a mind's for that mind. `auth`
pipes the header to curl, so the token is in no command line:

```bash
api=127.0.0.1:7878/v1/minds
auth() { printf 'authorization: Bearer %s\n' "$1"; }
auth "$MENSOR_TOKEN" | curl -s -X POST $api/m/prompt -H @- -H 'content-type: application/json' \
  -d '{"text":"Write a haiku in b2. Then fork from b2: one child translates it into French, one into German. Wait for both and pick one."}'
auth "$MENSOR_TOKEN" | curl -s -X POST $api/n/prompt -H @- -H 'content-type: application/json' \
  -d '{"text":"In c3, write a limerick to limerick.txt."}'
docker compose exec b2 find work work.clones                                       # the fork's clones
auth "$MENSOR_N_TOKEN" | curl -s $api -H @-                                        # n, and no m
auth "$MENSOR_N_TOKEN" | curl -s -o /dev/null -w '%{http_code}\n' $api/m -H @-   # 403
```

Histories live in the volume `store`; a hand's directory and clones live in
its container. Each container runs mensor as PID 1 with no init, on
purpose: an init would hold the token in its environment, where commands
could read it. So a job a command leaves running, once it exits, stays a
zombie until its hand ends.

## Postgres

[`compose.postgres.yaml`](compose.postgres.yaml) adds a Postgres container
and keeps the histories there. The image has `postgres` unless built with
`--build-arg FEATURES=`.

```bash
docker compose down
export COMPOSE_FILE=examples/docker/compose.yaml:examples/docker/compose.postgres.yaml
export MENSOR_DB_PASSWORD="$(openssl rand -hex 24)"
docker compose up -d
```

The host reaches it with `sslmode=disable`: it has no certificate, and its
traffic stays on the compose network. Talk to `m` and `n` as above, then
ask the database what each mind's hands have run:

```bash
docker compose exec postgres psql -U mensor -c "
  SELECT mind, item->>'hand' AS hand, item->>'operation' AS tool, count(*)
  FROM items WHERE kind = 'call' GROUP BY 1, 2, 3 ORDER BY 1, 2, 3"
docker compose run --rm host serve   # refused: another mensor host holds it
docker compose restart host          # m, n and their children carry on
```
