# Security

What mensor keeps from the commands a model runs, and who may reach a mind.

## Commands

A hand is not a sandbox: a command reaches whatever the hand's process can,
so confine the hand. `mensor run` puts commands beside the model's key;
elsewhere run the host and each hand apart, or in containers as
[examples/docker](../examples/docker) does.

Commands start without the host's token, model keys or the store's URL. On
Linux every mensor process makes itself non-dumpable as it starts, so its
user's other processes, commands included, can neither read its
`/proc/<pid>/environ` and memory nor attach a debugger to it. A hand's jobs
die with it. Still open:

- a process of the same user already running, which can catch a binary as
  it starts;
- jobs left by a hand killed with SIGKILL;
- root with `CAP_SYS_PTRACE`, and macOS, where a process can read its
  user's other processes' environments;
- whatever holds the variables around mensor: a shell script, an init such
  as tini, a `docker exec` shell.

## Tokens

With `MENSOR_TOKEN` set, every request needs `Authorization: Bearer <token>`,
and a hand sends its own `MENSOR_TOKEN`. The host's reaches every mind;
give a mind's hands one for it alone:

```bash
hand_token="$(mensor token --mind m)"   # MENSOR_TOKEN holds the host's
MENSOR_TOKEN="$hand_token" mensor hand --mind m --hand a1 --url wss://mensor.example/v1/attach
```

A token is as strong as a shell on every hand of the minds it reaches: it
can prompt them. A token for `m`:

- reaches `m` and all its children: it can prompt and interrupt them,
  release their hands, read their histories, and connect as any of their
  hands, replacing it;
- gets 403 for any other mind, and a hand that uses it for one is closed
  with 4006. `GET /v1/minds` lists only `m`'s, and `PUT` may make a mind `from` only one it reaches;
- is stored nowhere: a new `MENSOR_TOKEN` ends them all;
- separates only hands run as another user or machine: a hand of the
  host's user can read its store;
- belongs outside a hand's directory, where the model reads.

## Without a token

The host trusts every process on its machine. It listens only on loopback
and answers 403 to a `Host` that is not `localhost` or a loopback address,
or an `Origin` that is sent and is not, so a web page can't reach it by DNS
rebinding.

Off loopback, set `MENSOR_TOKEN`, put TLS in front, and have hands dial
`wss://`. A Postgres store is reached over TLS unless its URL says
`sslmode=disable`: see [postgres.md](postgres.md).
