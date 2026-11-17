# A hand of your own

[`hand.py`](hand.py) is a hand that rolls dice. It speaks
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

The model is shown one more tool, `roll`. With `MENSOR_TOKEN` set, the hand
sends it, as `mensor hand` does: the host's token, or one `mensor token`
made for its mind.

To stay short, it leaves out what lets a hand outlive its connection: it
does not dial again, and holds no result for a later hello. Run again, it is
a new boot, so the mind takes a call it left unfinished as unknown.
`crates/hand-host` does all of that.
