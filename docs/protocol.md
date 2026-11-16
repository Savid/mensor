# Protocol

What a mind and its hands say to each other. `mensor-protocol` implements it.

A mind has several hands, and each is joined to it by a connection of its
own. The protocol is what one hand and its mind say over theirs, and keeps
nothing itself. Every guarantee comes from what the two ends remember: the
mind its history, the hand the ids it has read and the results it holds.

## Transport

- The protocol names no transport. Over a network the binding is a
  WebSocket: the hand dials `GET /v1/attach` on the mind's host, and each
  message is one text frame. A hand in its mind's own process, or in one
  object with it, passes the same messages in memory, and the rest of this
  section does not apply to it.
- A host with a token requires `Authorization: Bearer <token>` on the
  upgrade, and answers 401 without it.
- One connection per mind and hand. A newer one replaces the older.
- Each message is one JSON object with a `type`. Fields are camelCase. A
  field a reader does not know is ignored.
- The hand speaks first, with a `hello`, within 10 s. The mind sends nothing
  until it has settled that hello.
- The hand pings every 20 s. A host drops a connection silent for 60 s.

## Hand to mind

**`hello`**, first on every connection:

```json
{"type":"hello","mind":"m","hand":"a1","boot":"9f3c2a…","lastId":41,
 "running":[41],"held":[{"id":40,"output":"…"}],"capabilities":[…]}
```

| Field | Meaning |
| --- | --- |
| `mind` | The mind the hand joins. |
| `hand` | The hand's name, unique within its mind. |
| `boot` | Random per hand process. A hand that restarted has a new one and remembers no earlier call. |
| `lastId` | The highest call id the hand has read, or 0. |
| `running` | The calls still running. |
| `held` | The results not yet acked, oldest first. |
| `capabilities` | What the hand can do: each a `name` and its `operations`, each of those a `name`, a `description` and the JSON Schema of its `input`. |

A name is 1 to 64 letters, digits, `-`, `_` or `.`, starting with a letter
or digit.

**`result`**, a call finished. It carries `output`, any JSON the operation
returned, or `error`, a string:

```json
{"type":"result","id":41,"output":{"exit":0,"stdout":"ok\n","stderr":""}}
{"type":"result","id":42,"error":"this hand has no capability named fetch"}
```

**`refused`** `{"type":"refused","id":41,"reason":"…"}`: a call whose id is
not above `lastId`. It did not start. The host drops the connection, and the
next hello settles the call.

## Mind to hand

**`call`**, do one thing:

```json
{"type":"call","id":42,"capability":"commands","operation":"bash","input":{"command":"cargo test"}}
```

**`cancel`** `{"type":"cancel","id":42}`: stop a running call. Its result
follows, as an error.

**`ack`** `{"type":"ack","ids":[40,41]}`: these results are stored, and the
hand may forget them.

## Delivery

- **At most once.** The mind stores a call before it sends it, under ids
  that only grow. One mind numbers the calls of all its hands, so each hand
  sees ids that rise. The hand starts a call only if its id is above
  `lastId`.
- **At least once.** The hand sends a result when its call ends, if it is
  connected, and repeats it in every hello until it is acked. The mind stores
  the first result for a call, taken from the hand it was sent to, and only
  acks any other.
- **Calls outlive connections.** A dropped connection stops nothing. The hand
  dials again, waiting 100 ms and doubling up to 5 s, each wait cut by a
  random part.
- **Retention.** A hand holds at most 16 MiB less 64 KiB of results. Past
  that the oldest go, and settle as unknown. One result larger than that
  becomes an error that says so.

## Settling a hello

`pending` is every call the mind sent this hand that has no outcome yet.

1. **The boot changed** from the one the mind last stored for this hand: the
   hand restarted and remembers none of the pending calls. Each may have
   run, so each is recorded as unknown, and nothing is sent again.
2. Otherwise, for each pending call, the first row that matches:

   | The pending call is… | The mind |
   | --- | --- |
   | in `held` | takes that result |
   | in `running` | waits for its result |
   | above `lastId` | sends it again under the same id: it never arrived |
   | at or below `lastId`, in neither | records its outcome as unknown: it may have run |

3. Once what it took is stored, the mind acks every id in `held`.

An unknown outcome reaches the model as an error that says the call may have
run, and to check the current state before running it again.

A call is sent only to a hand that is connected. One the model asks of a
hand that is away waits, unsent, until the hand says hello, and an interrupt
meanwhile ends it as not run.

## Minds and names

A host decides what a hello means from its `mind`:

| The mind is… | The host |
| --- | --- |
| one it knows | gives it the hand: a hand it knows is reconnecting, a new one joins |
| new, with no `.` in its name | starts it, with an empty history |
| new, with a `.` in its name | refuses for now: it is a fork's child, not made yet |

A mind refuses a hand it has released, and a new one once it has all the
hands it may.

A mind makes its children itself, when its model forks. It asks one of its
hands to clone itself with a `call` to the hand's `clone` capability:

```json
{"type":"call","id":43,"capability":"clone","operation":"clone",
 "input":{"into":[{"mind":"m.k3x9q2ab","hand":"a1"},{"mind":"m.p8d1rx0q","hand":"a1"}]}}
```

The hand copies itself once for each target and starts a hand on each copy,
which says hello to that mind under that name. Every copy is made or none.
When the result is an output, the host makes each child, sharing its
parent's history up to that call, and only then stores the fork. A clone
that dialled before its child was made was refused for now, and dials
again. A child's name is its parent's root, the part before the first `.`,
and a code: no longer than 64 bytes at any depth, so a mind that starts
from nothing has a name of at most 55 bytes.

A `clone` into the hand's own mind, under a new name, makes another hand of
that mind: the mind's `clone_hand`.

## Refusals

A host that turns a hand away closes the WebSocket with one of these codes.
A hand dials again only after one that is not final.

| Code | Meaning | Final |
| --- | --- | --- |
| 4000 | The hand sent something the protocol does not define | yes |
| 4001 | The mind has all the hands it may | yes |
| 4002 | The mind is a fork's child that is not made yet | no: the hand dials again, and gives up after 2 minutes |
| 4003 | A newer connection has this mind and hand | yes |
| 4004 | The mind released this hand | yes: the hand's process ends |
| 4005 | A new mind's name is longer than 55 bytes | yes |

## Limits

| Limit | Value |
| --- | --- |
| One message | 16 MiB |
| Held results | 16 MiB less 64 KiB, as written in messages |
| A name | 64 bytes, and 55 for a mind that starts from nothing |
