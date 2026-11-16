# Protocol v1

What a mind and a hand say to each other. `mensor-protocol` implements it.

The protocol joins one mind to one hand and keeps nothing itself. Every
guarantee comes from what the two ends remember: the mind its history, the
hand the ids it has read and the results it holds.

## Transport

- The protocol names no transport. Over a network the binding is a
  WebSocket: the hand dials `GET /v1/attach` on the mind's host, and each
  message is one text frame. A hand in its mind's own process, or in one
  object with it, passes the same messages in memory, and the rest of this
  section does not apply to it.
- A host with a token requires `Authorization: Bearer <token>` on the
  upgrade, and answers 401 without it.
- One connection per name. A newer one replaces the older.
- Each message is one JSON object with a `type`. Fields are camelCase. A
  field a reader does not know is ignored.
- The hand speaks first, with a `hello`, within 10 s. The mind sends nothing
  until it has settled that hello.
- The hand pings every 20 s. A host drops a connection silent for 60 s.

## Hand to mind

**`hello`**, first on every connection:

```json
{"type":"hello","v":1,"name":"b7","was":"a1","boot":"9f3c2a…","lastId":41,
 "running":[41],"held":[{"id":40,"output":"…"}],"capabilities":[…]}
```

| Field | Meaning |
| --- | --- |
| `v` | The protocol version. A host refuses another before it reads the rest. |
| `name` | The hand's name: 1 to 64 letters, digits, `-`, `_` or `.`, starting with a letter or digit. |
| `was` | The name the hand had before it was cloned and fed this one. |
| `boot` | Random per hand process. A clone made with its memory keeps it. A hand that restarted has a new one and remembers no earlier call. |
| `lastId` | The highest call id the hand has read, or 0. |
| `running` | The calls still running. |
| `held` | The results not yet acked, oldest first. |
| `capabilities` | What the hand can do: each a `name` and its `operations`, each of those a `name`, a `description` and the JSON Schema of its `input`. |

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

- **At most once.** The mind stores a call before it sends it, under ids that
  only grow. The hand starts a call only if its id is above `lastId`.
- **At least once.** The hand sends a result when its call ends, if it is
  connected, and repeats it in every hello until it is acked. The mind stores
  the first result for a call and only acks any other.
- **Calls outlive connections.** A dropped connection stops nothing. The hand
  dials again, waiting 100 ms and doubling up to 5 s, each wait cut by a
  random part.
- **Retention.** A hand holds at most 16 MiB less 64 KiB of results. Past
  that the oldest go, and settle as unknown. One result larger than that
  becomes an error that says so.

## Settling a hello

`pending` is every call in the mind's history with no outcome yet.

1. **The boot changed** from the one the mind last stored: the hand restarted
   and remembers none of the pending calls. Each may have run, so each is
   recorded as unknown, and nothing is sent again.
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

## Names, clones and forks

A host decides what a hello means from its `name` and `was`:

| The name is… | The host |
| --- | --- |
| one it knows | attaches the connection to that pair, and settles it |
| new, with no `was` | starts a pair with an empty history |
| new, with a `was` it knows | forks that pair's mind for the clone |
| new, with a `was` it does not know | refuses for now: the original may yet attach |

Whoever copies a hand must feed the copy a new name. A copy that dials under
the old name is a reconnect, and takes the original's place.

A forked mind shares its parent's history up to a point the clone's hello
fixes:

- **The clone kept its memory** (its `boot` is one the history records). It
  shares everything up to the call `lastId` names. If the clone had finished
  with that call (it neither runs it nor holds its result), it also shares
  what followed, up to the next call or the next boot. If it has read no call,
  it shares everything before the first call made since its boot.
- **The clone remembers nothing** (its `boot` is new). It shares everything,
  and every pending call settles as unknown.

Then, in the new mind: tool uses that had not yet become calls are recorded
as not run, a `paused` item is stored, and the hello is settled as above. The
mind asks no model until it is prompted.

## Refusals

A host that turns a hand away closes the WebSocket with one of these codes.
A hand dials again only after one that is not final.

| Code | Meaning | Final |
| --- | --- | --- |
| 4000 | The hand sent something the protocol does not define | yes |
| 4001 | The hand speaks another version | yes |
| 4002 | The lineage has all the clones it may | yes |
| 4003 | `was` names a hand the host does not know | no |
| 4004 | A newer connection has this name | yes |
| 4005 | Not a clone of the hand it names: it has read a call that hand's mind never made | yes |

## Limits

| Limit | Value |
| --- | --- |
| One message | 16 MiB |
| Held results | 16 MiB less 64 KiB, as written in messages |
| A name | 64 bytes |
