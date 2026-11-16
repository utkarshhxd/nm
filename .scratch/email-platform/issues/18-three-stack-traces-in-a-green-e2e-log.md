Status: needs-triage

# Three stack traces sit in a passing e2e log

Found in the M1b whole-branch review and seen again in the whole-branch fix pass. Harmless and
pre-existing. Recorded because a stack trace in a green log is a thing a later reader misreads —
and because the next person who is debugging a real e2e failure will spend time on these first.

## What it looks like

Between tests 123 and 124 of a **passing** run (`126 passed`, exit 0):

```
11:12:44 AM [vite] http proxy error: /api/v1/me
Error: connect ECONNREFUSED 127.0.0.1:8080
    at TCPConnectWrap.afterConnect [as oncomplete] (node:net:1610:16)
    at TCPConnectWrap.callbackTrampoline (node:internal/async_hooks:130:17)
```

Three times, all for `/api/v1/me`, all at the same second.

## Why it happens

Two servers are involved in one run:

- `web/e2e/support/server.ts` starts the real `nm serve` on **8090**, and that is `baseURL` for
  every test in `e2e/`.
- `web/e2e/support/app.ts`'s worker-scoped `app` fixture starts a `vite preview` of its own for
  the component and theme tests, and `web/vite.config.ts` proxies `/api` to **8080**
  (`proxy: { "/api": "http://127.0.0.1:8080" }`) — the port a developer's own `scripts/dev-serve.sh`
  listens on, and one nothing in the e2e run ever starts.

So when the theme tests load the real `index.html` through that preview server, the app boots and
asks for `/api/v1/me`, vite tries 8080, and nothing is there. The app renders signed-out, which is
all those tests need, and they pass.

## Why it is not a bug to fix blindly

Nothing is broken. The fetch failing is the *correct* precondition for a test about pre-paint
theming: there is no session, so nothing in the shell depends on an answer. Pointing the preview
proxy at 8090 would make those three tests load a real signed-out session from the e2e instance —
a different precondition, quietly, and one that couples the component suite to the server suite it
was deliberately built not to need (`app.ts`: "Needs no database and no `nm` binary").

## Options, cheapest first

1. Silence it where it is raised: give the `preview` server in `app.ts` its own
   `proxy: { "/api": { target: …, configure: … } }` that answers `/api/v1/me` with `401` instead
   of forwarding. The failure becomes the intended precondition, stated.
2. Leave it and write the explanation into `app.ts`, so the next reader of a green log finds the
   answer next to the cause rather than in this file.

Either is fine. What should not happen is the proxy being repointed at the e2e server to make the
log quiet.

## Comments
