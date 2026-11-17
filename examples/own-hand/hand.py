# /// script
# requires-python = ">=3.11"
# dependencies = ["websockets>=14"]
# ///
"""A hand of your own, that rolls dice for a mind.

It speaks docs/protocol.md and nothing more. It does not dial again: when its
connection ends, so does it, and run again it is a new boot, so the mind
takes a call it left unfinished as unknown.
"""

import argparse
import asyncio
import json
import os
import random
import secrets

from websockets.asyncio.client import connect
from websockets.exceptions import ConnectionClosed

DICE = {
    "name": "dice",
    "operations": [
        {
            "name": "roll",
            "description": "Roll dice. Returns each roll and the total.",
            "input": {
                "type": "object",
                "properties": {
                    "count": {"type": "integer", "minimum": 1, "maximum": 100},
                    "sides": {"type": "integer", "minimum": 2, "maximum": 1000},
                },
                "required": ["count", "sides"],
            },
        }
    ],
}


def run(call):
    if (call["capability"], call["operation"]) != ("dice", "roll"):
        raise ValueError(f"this hand cannot {call['capability']}.{call['operation']}")
    count, sides = call["input"]["count"], call["input"]["sides"]
    if not (isinstance(count, int) and 1 <= count <= 100):
        raise ValueError("count is a whole number from 1 to 100")
    if not (isinstance(sides, int) and 2 <= sides <= 1000):
        raise ValueError("sides is a whole number from 2 to 1000")
    rolls = [random.randint(1, sides) for _ in range(count)]
    return {"rolls": rolls, "total": sum(rolls)}


async def main(args):
    token = os.environ.get("MENSOR_TOKEN")
    headers = {"Authorization": f"Bearer {token}"} if token else {}
    # websockets pings every 20 s, as the protocol asks.
    async with connect(args.url, additional_headers=headers) as mind:
        hello = {
            "type": "hello",
            "mind": args.mind,
            "hand": args.hand,
            "boot": secrets.token_hex(8),
            "lastId": 0,
            "running": [],
            "held": [],
            "capabilities": [DICE],
        }
        await mind.send(json.dumps(hello))
        print(f"{args.hand} said hello to {args.mind}")
        try:
            async for frame in mind:
                call = json.loads(frame)
                if call["type"] != "call":
                    continue  # an ack or a cancel: a roll keeps nothing and ends at once
                try:
                    result = {"type": "result", "id": call["id"], "output": run(call)}
                except (KeyError, TypeError, ValueError) as error:
                    result = {"type": "result", "id": call["id"], "error": f"bad call: {error!r}"}
                print(call["id"], call["input"], "->", result.get("output") or result["error"])
                await mind.send(json.dumps(result))
        except ConnectionClosed:
            pass
        print("the connection ended:", mind.close_code, mind.close_reason)


parser = argparse.ArgumentParser(description="A hand that rolls dice for a mind.")
parser.add_argument("--url", default="ws://127.0.0.1:7878/v1/attach", help="where hands attach")
parser.add_argument("--mind", required=True, help="the mind to join")
parser.add_argument("--hand", required=True, help="this hand's name in its mind")
asyncio.run(main(parser.parse_args()))
