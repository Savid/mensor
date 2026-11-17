# /// script
# requires-python = ">=3.11"
# dependencies = ["aiohttp>=3.9"]
# ///
"""opencode as a hand of a mind. A demo: do not run it for real.

opencode takes this for its model, at http://127.0.0.1:7979/v1. On opencode's
first request the shim joins the mind as a hand whose tools are opencode's.
It answers each request with the next call the mind sends, as a tool call,
and passes the tool result in opencode's next request back to the mind.
"""

import argparse
import asyncio
import json
import os
import secrets

import aiohttp
from aiohttp import web

calls = asyncio.Queue()  # calls from the mind, not yet given to opencode
running = {}  # tool call id -> the mind's call id
mind = None  # the WebSocket to the mind


async def join(tools):
    token = os.environ.get("MENSOR_TOKEN")
    headers = {"Authorization": f"Bearer {token}"} if token else {}
    socket = await aiohttp.ClientSession().ws_connect(args.url, headers=headers, heartbeat=20)
    operations = [
        {
            "name": tool["function"]["name"],
            "description": tool["function"].get("description", ""),
            "input": tool["function"]["parameters"],
        }
        for tool in tools
        if tool["function"]["name"] != "task"  # a sub-agent would be a second conversation
    ]
    hello = {
        "type": "hello",
        "mind": args.mind,
        "hand": args.hand,
        "boot": secrets.token_hex(8),
        "lastId": 0,
        "running": [],
        "held": [],
        "capabilities": [{"name": "opencode", "operations": operations}],
    }
    await socket.send_json(hello)
    print(f"{args.hand} joined {args.mind} with", ", ".join(op["name"] for op in operations))
    asyncio.create_task(listen(socket))
    return socket


async def listen(socket):
    async for frame in socket:
        message = json.loads(frame.data)
        if message["type"] == "call":
            print(f"call {message['id']}: {message['operation']} {json.dumps(message['input'])[:100]}")
            await calls.put(message)
    print("the mind closed the connection:", socket.close_code)


@web.middleware
async def here_only(request, handler):
    """As mensor's host with no token: a loopback Host, and no browser's Origin."""
    host = request.host.rsplit(":", 1)[0].strip("[]")
    if host not in ("localhost", "127.0.0.1", "::1") or "Origin" in request.headers:
        raise web.HTTPForbidden(text="this model serves only its own machine")
    return await handler(request)


async def completions(request):
    global mind
    body = await request.json()
    reply = web.StreamResponse(headers={"Content-Type": "text/event-stream"})
    await reply.prepare(request)
    if not body.get("tools"):
        # opencode asks its model for a session title too.
        return await answer(reply, {"content": "a mensor hand"}, "stop")
    if mind is None:
        mind = await join(body["tools"])

    # Every request carries the whole conversation: pass on the new results.
    for message in body["messages"]:
        if message["role"] == "tool" and message["tool_call_id"] in running:
            id = running.pop(message["tool_call_id"])
            print(f"result {id}: {message['content'][:100]!r}")
            await mind.send_json({"type": "result", "id": id, "output": message["content"]})

    # Hold the request open until the mind has a call for this hand.
    while True:
        try:
            call = await asyncio.wait_for(calls.get(), 10)
            break
        except TimeoutError:
            await reply.write(b": waiting for the mind\n\n")
    running[f"call_{call['id']}"] = call["id"]
    tool_call = {
        "index": 0,
        "id": f"call_{call['id']}",
        "type": "function",
        "function": {"name": call["operation"], "arguments": json.dumps(call["input"])},
    }
    return await answer(reply, {"tool_calls": [tool_call]}, "tool_calls")


async def answer(reply, delta, finish):
    """Stream one message, saying it used almost no tokens, so opencode never compacts."""
    usage = {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2}
    for chunk in (
        {"choices": [{"index": 0, "delta": {"role": "assistant", **delta}}]},
        {"choices": [{"index": 0, "delta": {}, "finish_reason": finish}], "usage": usage},
    ):
        await reply.write(f"data: {json.dumps(chunk)}\n\n".encode())
    await reply.write(b"data: [DONE]\n\n")
    return reply


parser = argparse.ArgumentParser(description="opencode as a hand of a mind: a demo.")
parser.add_argument("--url", default="ws://127.0.0.1:7878/v1/attach", help="where hands attach")
parser.add_argument("--mind", required=True, help="the mind to join")
parser.add_argument("--hand", required=True, help="this hand's name in its mind")
args = parser.parse_args()

app = web.Application(client_max_size=64 * 2**20, middlewares=[here_only])
app.router.add_post("/v1/chat/completions", completions)
print("opencode's model is at http://127.0.0.1:7979/v1")
web.run_app(app, host="127.0.0.1", port=7979, print=None)
