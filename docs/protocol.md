# Protocol

What a mind and its hands say to each other, each hand over its own
connection; `mensor-protocol` implements it. The protocol keeps nothing:
the mind remembers its history, the hand the call ids it read and the
results it holds.

## Transport

- A hand dials `GET /v1/attach` on the host as a WebSocket, one message per
  text frame. In the mind's own process or object, the same messages pass
  in memory.
- With a token, the upgrade needs `Authorization: Bearer <token>`, or gets
  401. Without one, the host answers only a loopback `Host`, and a loopback
  `Origin` if one is sent, else 403.
- A message is a JSON object with a `type`, its fields camelCase. Unknown
  fields are ignored.
- The hand sends `hello` within 10 s, and the mind sends nothing until it
  has settled it. The hand sends a WebSocket ping every 20 s; the host drops
  a connection silent for 60 s. A newer connection for a mind and hand
  replaces the older.

## Messages

| From | Message | Means |
| --- | --- | --- |
| hand | `hello` | First on every connection: who it is, and where it stands. |
| hand | `result` `{"id", "output"}` or `{"id", "error"}` | A call finished. `output` is any JSON; `error` a string. |
| hand | `refused` `{"id", "reason"}` | A call not above `lastId`, not started. The mind never sends such a call, so the host closes with 4000. |
| mind | `call` `{"id", "capability", "operation", "input"}` | Do one thing. A hand gets one call at a time. |
| mind | `cancel` `{"id"}` | Stop a running call. Its result follows, as an error. |
| mind | `ack` `{"ids"}` | These results are stored: forget them. |

| `hello` field | |
| --- | --- |
| `mind`, `hand` | The mind it joins, and its name there: 1 to 64 ASCII letters, digits, `-`, `_` or `.`, starting with a letter or digit. |
| `boot` | Random per hand process. A new one says the hand remembers no earlier call. |
| `lastId` | The highest call id it has read, or 0. |
| `running` | The ids of the calls still running. |
| `held` | The results not yet acked, each as in `result`, oldest first. |
| `capabilities` | What it can do, below. A later hello's capabilities replace the earlier ones from the mind's next step. |

```json
{"type":"hello","mind":"m","hand":"dice","boot":"9f3c2a…","lastId":0,"running":[],"held":[],
 "capabilities":[{"name":"dice","operations":[{"name":"roll","description":"Roll dice.",
   "input":{"type":"object","properties":{"count":{"type":"integer"}},"required":["count"]}}]}]}
{"type":"call","id":43,"capability":"dice","operation":"roll","input":{"count":3}}
{"type":"result","id":43,"output":{"rolls":[6,5,6],"total":17}}
```

Each operation becomes a tool the model can call, its `input` a JSON
Schema, which must not have a `hand` property: the mind adds its own. The
least a hand can do is the above: a fresh `boot`, `lastId` 0, empty lists,
and a result for each call. Run again, it has a new boot, so the mind
records the calls it left unfinished as unknown.

## Delivery

![A hand says hello with the last id it read. The mind stores each call before it sends it, one call at a time to a hand, and acks each result once stored. The connection drops while call 42 runs; the next hello carries its held result.](images/protocol.svg)

- **At most once.** The mind stores a call before sending it, under ids
  that only grow across all its hands. A hand starts a call only above
  `lastId`.
- **At least once.** The hand sends a result when its call ends, and
  repeats it in every hello until acked. The mind stores the first result
  from the hand it sent the call to.
- **Calls outlive connections.** A dropped connection stops nothing: the
  hand dials again. mensor's hands wait 100 ms, doubling to 5 s, jittered.
- **Retention.** One message is at most 16 MiB. mensor's hands hold 16 MiB
  less 64 KiB of results, and drop the oldest past that, which then settle
  as unknown.

## Settling a hello

A hello settles each call the mind sent that hand with no outcome yet:

| If | The mind |
| --- | --- |
| the `boot` differs from the last one stored for this hand | records every such call as unknown, and sends none again |
| the call is in `held` | takes that result |
| it is in `running` | waits for its result, or cancels it if the turn was interrupted meanwhile |
| its id is above `lastId` | sends it again under the same id, or records it as not run if the turn was interrupted meanwhile |
| otherwise: read, and gone | records it as unknown |

The first row that matches decides. Then the mind acks every id in `held`.
The model is told an unknown call may have run, and to check before
running it again. A call to a hand that is away waits, unsent, for its
hello.

## Minds and names

A hello to a mind the host knows joins it, a new hand or a returning one.
A hello to an unknown name with no `.`, of at most 55 bytes, starts that
mind with an empty history. Anything else is closed with a code below.

**Forks.** A mind makes its children by asking a hand to clone itself:

```json
{"type":"call","id":44,"capability":"clone","operation":"clone",
 "input":{"into":[{"mind":"m.d3342iuj","hand":"a1"},{"mind":"m.ckj6ldqi","hand":"a1"}]}}
```

The hand copies itself once per target, all or none, and each copy says
hello to its mind. On success the host makes each child, sharing the
history up to the call, and only then stores the fork. A child's name is
its root's, a `.` and a code: 64 bytes at most. A `clone` into the hand's
own mind is `clone_hand`.

**Made `from`.** A mind made with `PUT` and `from` keeps one of the other
mind's hands, whose hello settles as any other. Its call ids start above
every one the other mind issued, so a hand restored from later reads them
as new.

## Close codes

| Code | Why | The hand |
| --- | --- | --- |
| 4000 | It broke the protocol: no hello in 10 s, a second hello, a `refused`, an `input` with `hand`, an unknown message | ends |
| 4002 | Its mind is a fork's child not made yet | dials again, for up to 2 minutes |
| 4003 | A newer connection has this mind and hand | ends |
| 4004 | Its mind released it, or ended | ends |
| 4005 | A new mind's name is over 55 bytes | ends |
| 4006 | Its token was made for another mind | ends |
