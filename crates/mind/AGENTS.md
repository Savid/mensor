# mensor-mind

The mind as a pure state machine: the history, the turn loop, the tools and
forks, and the `Store` trait (`src/store.rs`).

- `src/tools.rs` is what the model is shown and what each tool does. A tool
  use for a hand that is away waits unissued. The mind's own tools run after
  their step's hand tools, one at a time, and a fork runs alone among them.
- Child names and fork budgets are decided here (`src/fork.rs`), never in a
  host.
- Minds talk only to their parent and children. `Output::Tell` is a prompt
  under a key made from the tool use, so a message sent again is the same
  prompt. `Output::Watch` lives in the host's memory only: a mind watches
  again when it loads (`Mind::start`).
- A mind holds only the end of its history: everything after its last
  summary, which carries what it needs from before (`from`, `hands`,
  `lastCall`, `forksLeft`, `parent`, `children`). What reaches further back
  asks the store, a page at a time.
- What a mind looks up (a call, an outcome, the last step) is in the round
  in progress. Only a prompt's key and the last call's id are looked for
  further back.
