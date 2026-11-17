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
