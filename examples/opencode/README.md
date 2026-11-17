# opencode as a hand

A demo, not something to run for real. [opencode](https://opencode.ai) takes
[`shim.py`](shim.py) for its model. The shim joins a mind as a hand whose
tools are opencode's, and the mind drives opencode one tool call at a time:

| mensor | What opencode sees |
| --- | --- |
| The hand's capabilities | The tools in its request |
| A `call` | Its model asking for a tool |
| A `result` | The tool's result, in its next request |

Start a host as in the [quick start](../../README.md#quick-start), then the
shim, then opencode in a directory it may change, each in its own shell:

```bash
uv run examples/opencode/shim.py --mind m --hand opencode
mkdir -p /tmp/oc && env -i PATH="$PATH" HOME="$HOME" \
  OPENCODE_CONFIG="$PWD/examples/opencode/opencode.json" OPENCODE_EXPERIMENTAL_LSP_TOOL=1 \
  opencode run --dir /tmp/oc "You are a hand."
curl -s -X POST 127.0.0.1:7878/v1/minds/m/prompt -H 'content-type: application/json' \
  -d '{"text":"Write fib.py and run it. Then find every reference to fib with the lsp tool."}'
```

[`opencode.json`](opencode.json) makes the shim opencode's model and turns
on its language servers. `env -i` keeps your credentials out of the commands
the mind runs. Like a host with no token, the shim answers only a loopback
`Host` and no browser's `Origin`, so a web page cannot send it results.

The mind gets what opencode's tools do and mensor's don't: `lsp` finds
definitions and references, and an `edit` that breaks a type says so:

```
Edit applied successfully.

LSP errors detected in this file, please fix:
ERROR [17:12] Type "str" is not assignable to return type "int"
```

What the demo leaves out:

- The mind never sees opencode's system prompt, so not its directory or its
  `AGENTS.md`. It finds its way with `bash` and `glob`.
- A call cannot be cancelled: it runs to its end.
- opencode's `task`, a sub-agent, is hidden: it would be a second
  conversation with the shim.
- Nothing is held for a later hello. If the shim or opencode stops, the call
  it was running is unknown.
