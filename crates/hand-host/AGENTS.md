# mensor-hand-host

A hand on a machine: the driver, the dial, and the capabilities that touch
the machine.

- A hand dials through a `Dial`, which returns a `Link`. `WebSocket` dials
  over a network, and `crates/mensor/src/local.rs` in memory.
- `clone` (`src/capabilities/clone.rs`) is never shown to the model. It
  makes every copy or none, under a `.part` name until the copy is whole.
- `Commands` runs each command in a process group of its own, and kills
  every group it started when its last copy is dropped, which is when its
  hand ends. What a command leaves running outlives its call, never its
  hand.
