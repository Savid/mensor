# mensor-cloudflare

The Cloudflare worker: a mind and its hand `local` as one Durable Object,
over `crates/object`, its only host. See `docs/cloudflare.md`.

- Put what does not need the platform in `crates/object` or `src/api.rs`,
  where native tests reach it.
- A workerd test (`tests/workerd.rs`) starts the worker with `--var` values
  made for it, in an environment cleared down to `PATH`, `HOME` and
  `VOLTA_HOME`, with `CI` and `WRANGLER_SEND_METRICS` set.
