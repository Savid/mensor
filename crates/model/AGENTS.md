# mensor-model

Requests and answers for the Messages API and Chat Completions.

- This crate builds each API's requests and reads its answers
  (`src/messages.rs`, `src/chat.rs`), retries included. Hosts only send.
- A request is written from structs that borrow the conversation, never
  through a `serde_json::Value`.
- Every request streams, and `Reading` turns the events back into one
  answer.
