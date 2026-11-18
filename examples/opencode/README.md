# opencode as a hand

A demo. [opencode](https://opencode.ai) takes [`shim.py`](shim.py) for its
model; the shim joins a mind as a hand whose tools are opencode's, so the
mind drives opencode one tool call at a time:

| mensor | opencode sees |
| --- | --- |
| the hand's capabilities | the tools in its request |
| a `call` | its model asking for a tool |
| a `result` | the tool's result, in its next request |

Start a host as in the [quick start](../../README.md#quick-start), then
each in its own shell:

```bash
uv run examples/opencode/shim.py --mind m --hand opencode
mkdir -p /tmp/oc && env -i PATH="$PATH" HOME="$HOME" \
  OPENCODE_CONFIG="$PWD/examples/opencode/opencode.json" OPENCODE_EXPERIMENTAL_LSP_TOOL=1 \
  opencode run --dir /tmp/oc "You are a hand."
curl -s -X POST 127.0.0.1:7878/v1/minds/m/prompt -H 'content-type: application/json' \
  -d '{"text":"Write fib.py and run it. Then find every reference to fib with the lsp tool."}'
```

[`opencode.json`](opencode.json) makes the shim opencode's model and turns
on its language servers. `env -i` keeps your credentials from the commands.
The shim, like a host with no token, answers only a loopback `Host` and no
browser `Origin`.

The mind gains what mensor's tools lack: `lsp` finds definitions and
references, and an `edit` that breaks a type says so:

```
Edit applied successfully.

LSP errors detected in this file, please fix:
ERROR [17:12] Type "str" is not assignable to return type "int"
```

Left out: opencode's system prompt (the mind finds its way with `bash` and
`glob`), cancelling a call, opencode's `task` sub-agent, and holding a
result for a later hello, so a call running when the shim or opencode stops
is unknown.
