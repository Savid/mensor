# mensor-mind-host

The server: a task per mind, the attach endpoint, the API and the model
client.

- A mind's outputs are performed in order: `Output::Append` is stored and
  `Output::Fork` made before the outputs after them.
- `Store` calls never run on a runtime thread: they go through `blocking`
  (`src/task.rs`) or `spawn_blocking`.
- `Output::Tell` is delivered on a task of its own: a mind's task never
  awaits another mind's. `Host::open` wakes every mind with work that needs
  no hand. A resting mind's status comes from `Store::recent`, which
  includes the items that name its hands and kin.
- A hand reaches a host through `Host::connect`, which returns a
  `Connection`.
- What a mind tells its followers goes down one ordered channel each, so a
  reply's text always comes before the step that holds it.
- A mind is in memory only while it is awake. The way to reach its task is
  cloned only under the lock on the host's map of minds, and dropped once
  used, except by a hand's `Connection`, which holds it until the hand
  detaches. A task that rests relies on being the only holder.
- `guard` (`src/http.rs`) decides what a request reaches, with
  `mensor_api::Access`. A route that names a mind takes the name through
  `Reached`, never a bare `Path`, and attach checks the hello's mind before
  `Host::connect`.
- `Host::stop` ends the host as an exiting process would: each mind's task
  between commands, each hand's WebSocket with 1001. A task that lives as
  long as a mind or a connection selects on `stopped` (`src/task.rs`), so
  that what it holds, the store among it, is let go.
- A mind wakes with its record's settings over the host's (`settings` in
  `src/task.rs`), and a fork's child is made with its parent's.
- A mind asks the `Model` its own model names in `Config::models`, or the
  host's own (`Inner::asking`), so models behind several APIs share one
  host.
- `Host::make` refuses a model the host does not offer (its `Model`'s own,
  or one of `Config::models`) and an effort that model's `Model` does not
  take, checked on the settings a mind made from another ends up with. It
  decides a mind made from another's history with `Mind::branch`, over the
  store's `tail_at` and the other's last call, under the lock on the host's
  minds, so that nothing else makes or joins it meanwhile.
- A JSON body is read through `Body`, which refuses what it cannot read as
  a 400 problem, never axum's own rejection.
