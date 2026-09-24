# Bodega UI

This is where the web UI lives. It is a pure client of the
[Bodega API](../docs/API.md): no engine code, no database access, just
`fetch` and `EventSource`.

What is here today:

- [`src/api/types.ts`](src/api/types.ts): TypeScript types for every request,
  response and event, **generated from the Rust types**. Don't edit it by
  hand. After changing an API type in Rust, regenerate it with
  `UPDATE_TYPES=1 cargo test -p bodega-server --test typescript`
  (CI fails if you forget).

Develop against a live engine:

```sh
# terminal 1: an API with some runs (the mock agent spends no tokens)
cd some-repo && bodega init && git add -A && git commit -m bodega
bodega run "Try it" --agent mock
bodega serve --allow-origin http://localhost:5173

# terminal 2: your dev server on :5173
```

`bodega serve --ui <dist-dir>` serves a built UI from the same origin, so
production needs no CORS at all.

Ideas for views, based on what the research found is missing in every
existing tool (attention inbox, swarm timeline, run graph, replay,
verification badges) are at the end of [`docs/API.md`](../docs/API.md).
