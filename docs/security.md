# Security

What mensor keeps from the commands a model runs, and who may reach a mind.

## What commands see

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
process that holds the model's credential. To keep them apart, run the host
and each hand on their own, or each in a container as
[examples/docker](../examples/docker) does.

## Tokens

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
  `GET /v1/minds` lists only `m`'s, and a `PUT` may make a mind `from` only
  one it reaches;
- is kept nowhere by the host: a new `MENSOR_TOKEN` ends them all;
- separates only hands that run as another user or on another machine than
  the host: a hand of the host's own user can read its store.

Carry it as any secret, and never in a file in a hand's directory, where the
model reads.

## Without a token

Without `MENSOR_TOKEN`, the host trusts every process on its machine. It
listens only on loopback, and answers 403 when `Host` is not `localhost` or
a loopback address, or when a browser's `Origin` is not one either. That
stops browsers only: a web page can't reach the host by DNS rebinding, or
open a WebSocket to it as a hand.

## Off loopback

The host speaks plain HTTP: off loopback, set `MENSOR_TOKEN`, put TLS in
front of it, and have hands dial `wss://`. A Postgres store is reached over
TLS unless its URL says `sslmode=disable`: see [postgres.md](postgres.md).
