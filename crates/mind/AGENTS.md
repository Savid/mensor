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
- `src/suite.rs` is what every store does, as functions over a new, empty
  store, behind the `suite` feature. `store_suite!` writes a test for each:
  a new function joins its list there.
- A mind that has ended (`Item::Ended`) does nothing more: `next` is idle,
  it asks no model, takes no prompt and refuses every hand. Whatever reads
  hands, kin or the turn checks `ended` first.
- A mind made from another's history starts with what `Mind::branch` gives,
  as a fork's child starts with `Parted`: an unknown outcome for each clone
  in flight and each call on a hand it left, `Released` for those hands,
  then `Branched`, which resets its kin and numbers its calls above every
  one its keeper made.
- Which of the mind's own tools are offered is `Settings::tools`; when each
  applies is `Table::new`. A fork leaves a mind's tools as they were, and
  its children start with the same.
- What the window writes for an item depends on that item and those before
  it, never on later ones or on who reads it: names, not "your parent", and
  a summary's hands and kin as it stored them. A child sends its parent's
  last request as the start of its own. The two exceptions are items too:
  the last summary replaces what came before it, and the last trim after it
  (`Item::Trimmed`) shows older results as a note, so a request still
  depends only on the items before it, and changes once per summary or
  trim.
- A step's `under` is the fingerprint of the request that brought it
  (`ModelRequest::fingerprint`, taken from the request as sent). Its
  reasoning (`Block::Other`) goes back only to a request with the same.
  Anything that changes the model, system prompt or tools changes the
  fingerprint, and leaves out earlier reasoning.
