# azazel web

The browser front end of `azazel remote`: a SvelteKit 2 static SPA (Svelte 5 runes, TypeScript, Tailwind v4,
shadcn-svelte). It is a thin child: it sends RPC commands and draws frames. The session, agents, tools and
permissions live in the azazel process. The API it speaks is in [docs/remote.md](../docs/remote.md), the
protocol in [docs/rpc.md](../docs/rpc.md).

Use **bun**.

```sh
bun install
bun run gen:types   # src/lib/protocol/rpc.ts from docs/rpc.schema.json
bun run check       # svelte-check
bun run lint        # eslint + prettier --check
bun run test        # vitest: reducer over fixtures, markdown XSS, reconnect
bun run build       # web/build, with no inline scripts (the server's CSP forbids them)
```

## Developing without Rust

```sh
bun run dev:mock    # a stand-in for `azazel remote` on :7420, replaying fixtures/*.jsonl
bun run dev         # Vite on :5173, proxying /api (and the WebSocket) to :7420
```

Open `http://localhost:5173/#pair=ctl-demo` (full control) or `#pair=view-demo` (watch only). The mock
sessions are `approve` (an approval waits), `team` (sub-agents), `showcase` (thinking, diff, markdown),
`forms` (a form question) and `ended`.

Against the real server: `azazel remote`, then `AZAZEL_REMOTE=http://127.0.0.1:7420 bun run dev` and open
the link it prints, with the host replaced by `localhost:5173`.

## Fixtures

`fixtures/*.jsonl` are frames recorded from `azazel rpc` with the scripted provider (a `--features
test-support` build, `AZAZEL_MOCK_SCRIPT=tests/fixtures/scripts/<name>.json`), one JSON frame per line.
`models.json`, `commands.json` and `state.json` are recorded query answers. Record new ones the same way
rather than writing frames by hand.

## Layout

- `src/lib/rpc/client.ts`: the socket, command ids, attach with `since`, reconnect with backoff.
- `src/lib/session/reduce.ts`: pure frame → model reducer. `session.svelte.ts`: runes state around it.
- `src/lib/markdown.ts`: marked + highlight.js + DOMPurify. Model text is untrusted.
- `src/lib/components/`: transcript, tool cards, diff view, approval and question sheets, composer, pickers,
  agent hub, usage.
- `scripts/mock-host.ts`: the mock server. `scripts/externalize-boot.ts`: moves SvelteKit's inline start
  script into a file after the build.
