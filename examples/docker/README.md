# Hands in containers

A host, and hands for two minds: `a1` and `b2` for `m`, `c3` for `n`, each
in a container of its own. They share one image, which has `git`,
`ripgrep`, `python3`, `jq` and `curl` for the model's commands: add what
your work needs to the [`Dockerfile`](Dockerfile). The hands dial the host
over the compose network, and run the model's commands in their own
containers.

Only the host has the model's credential and the host's token. Each mind's
hands have a token made from it for that mind, which reaches the mind and
its children and no other, but the whole of them: whoever holds it can
prompt them, interrupt them, release their hands, read their histories with
every hand's output, and say hello as any of their hands, which takes that
hand's place. The model's commands cannot read it from the hand's process,
but a `docker exec` shell has it in its environment.

```bash
docker build -f examples/docker/Dockerfile -t mensor-example .
export COMPOSE_FILE=examples/docker/compose.yaml
export MENSOR_TOKEN="$(openssl rand -hex 32)"   # the host's: it reaches every mind
export MENSOR_M_TOKEN="$(docker run --rm -e MENSOR_TOKEN mensor-example token --mind m)"
export MENSOR_N_TOKEN="$(docker run --rm -e MENSOR_TOKEN mensor-example token --mind n)"
export ANTHROPIC_API_KEY="$(cat path/to/anthropic.key)"
docker compose up -d
```

`-e MENSOR_TOKEN` passes the variable by its name, so the token is in no
command line. For Chat Completions, set `OPENAI_API_KEY`, `OPENAI_BASE_URL`
and `MENSOR_MODEL` in place of `ANTHROPIC_API_KEY`: for OpenRouter,
`https://openrouter.ai/api/v1` and a model such as
`anthropic/claude-haiku-5.5`.

Then talk to it as in the [quick start](../../README.md#quick-start), with a
token: the host's, or a mind's for that mind.

```bash
api=127.0.0.1:7878/v1/minds
auth() { printf 'authorization: Bearer %s\n' "$1"; }
auth "$MENSOR_TOKEN" | curl -s -X POST $api/m/prompt -H @- -H 'content-type: application/json' \
  -d '{"text":"Write a haiku in b2. Then fork from b2: one child translates it into French, one into German. Wait for both and pick one."}'
auth "$MENSOR_TOKEN" | curl -s -X POST $api/n/prompt -H @- -H 'content-type: application/json' \
  -d '{"text":"In c3, write a limerick to limerick.txt."}'
docker compose exec b2 find work work.clones
```

`n`'s token reaches `n` alone:

```bash
auth "$MENSOR_N_TOKEN" | curl -s $api -H @-                                        # n, and no m
auth "$MENSOR_N_TOKEN" | curl -s -o /dev/null -w '%{http_code}\n' $api/m -H @-   # 403
```

A fork's children run on clones of `b2`, in its container, under
`work.clones`, and join with its token. The histories are kept in the volume
`store`, and a hand's directory and clones in its container, until it is
removed.

Each container runs mensor as PID 1, with no init, on purpose: an init such
as tini would hold the container's environment, the token in it, where the
model's commands could read it. So nothing reaps a job a command left
running once it ends: it stays a zombie until the hand ends.

## Postgres

[`compose.postgres.yaml`](compose.postgres.yaml) keeps the histories in
Postgres in place of SQLite: it adds a Postgres container, and builds the
host with the `postgres` feature.

```bash
docker compose down
export COMPOSE_FILE=examples/docker/compose.yaml:examples/docker/compose.postgres.yaml
export MENSOR_DB_PASSWORD="$(openssl rand -hex 24)"
docker compose up -d --build
```

The host reaches this Postgres plainly (`sslmode=disable`): it has no
certificate, and its traffic stays on the compose network. A database
anywhere else keeps TLS.

Talk to `m` and `n` as above. Then ask the database what each mind's hands
have run, while they run:

```bash
docker compose exec postgres psql -U mensor -c "
  SELECT mind, item->>'hand' AS hand, item->>'operation' AS tool, count(*)
  FROM items WHERE kind = 'call' GROUP BY 1, 2, 3 ORDER BY 1, 2, 3"
```

One host holds the database at a time, and minds outlive their host:

```bash
docker compose run --rm host serve   # refused: another mensor host holds it
docker compose restart host          # m, n and their children carry on
```
