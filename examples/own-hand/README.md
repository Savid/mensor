# A hand of your own

[`hand.py`](hand.py) is a hand that rolls dice, in Python. It speaks
[the protocol](../../docs/protocol.md) and nothing more: a hello with one
capability, `dice`, and a result for each call.

Start a host as in the [quick start](../../README.md#quick-start), then:

```bash
uv run examples/own-hand/hand.py --mind m --hand dice
curl -s -X POST 127.0.0.1:7878/v1/minds/m/prompt -H 'content-type: application/json' \
  -d '{"text":"Roll 3d6 for strength, then 4d6 and drop the lowest for dexterity."}'
```

```
dice said hello to m
1 {'count': 3, 'sides': 6} -> {'rolls': [6, 5, 6], 'total': 17}
2 {'count': 4, 'sides': 6} -> {'rolls': [2, 1, 3, 1], 'total': 7}
```

The model gains one tool, `roll`. With `MENSOR_TOKEN` set, the hand sends
it, as `mensor hand` does. To stay short it neither dials again nor holds a
result for a later hello, so a call it leaves unfinished is unknown;
`crates/hand-host` does both.
