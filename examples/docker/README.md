# Hands in containers

A host and two hands, `a1` and `b2`, each in a container of its own. They
share one image, which has `git`, `ripgrep`, `python3`, `jq` and `curl` for
the model's commands: add what your work needs to the
[`Dockerfile`](Dockerfile). The hands dial the host over the compose
network, and run the model's commands in their own containers.

Only the host has the model's credential and the host's token. The hands
have a token made from it for the mind `m`, which reaches `m` and its
children and no other mind, but the whole of them: whoever holds it can
prompt them, interrupt them, release their hands, read their histories with
every hand's output, and say hello as any of their hands, which takes that
hand's place. The model's commands cannot read it from the hand's process,
but a `docker exec` shell has it in its environment.

```bash
docker build -f examples/docker/Dockerfile -t mensor-example .
export MENSOR_TOKEN="$(openssl rand -hex 32)"   # the host's: it reaches every mind
export MENSOR_HAND_TOKEN="$(docker run --rm -e MENSOR_TOKEN mensor-example token --mind m)"
export ANTHROPIC_API_KEY="$(cat path/to/anthropic.key)"
docker compose -f examples/docker/compose.yaml up -d
```

`-e MENSOR_TOKEN` passes the variable by its name, so the token is in no
command line. For Chat Completions, set `OPENAI_API_KEY`, `OPENAI_BASE_URL`
and `MENSOR_MODEL` in place of `ANTHROPIC_API_KEY`: for OpenRouter,
`https://openrouter.ai/api/v1` and a model such as
`anthropic/claude-haiku-5.5`.

Then talk to it as in the [quick start](../../README.md#quick-start), with a
token: the host's, or the hands' one, which reaches `m` too.

```bash
api=127.0.0.1:7878/v1/minds
auth() { printf 'authorization: Bearer %s\n' "$MENSOR_TOKEN"; }
auth | curl -s -X POST $api/m/prompt -H @- -H 'content-type: application/json' \
  -d '{"text":"Write a haiku in b2. Then fork from b2: one child translates it into French, one into German. Wait for both and pick one."}'
docker compose -f examples/docker/compose.yaml exec b2 find work work.clones
```

A fork's children run on clones of `b2`, in its container, under
`work.clones`, and join with its token. The histories are kept in the volume
`store`, and a hand's directory and clones in its container, until it is
removed.

Each container runs mensor as PID 1, with no init, on purpose: an init such
as tini would hold the container's environment, the token in it, where the
model's commands could read it. So nothing reaps a job a command left
running once it ends: it stays a zombie until the hand ends.
