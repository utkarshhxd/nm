# M0 decision record

Decisions taken while building milestone M0 (branch `m0-foundations`, 53 commits, 16 tasks)
whose reasoning does not fit in a code comment and is not recoverable from the diff. Each entry
says what was decided, why, and what it costs if the decision was wrong.

This is the "why" behind code that otherwise looks arbitrary. If you are about to change
something listed here, read the entry first — several of these are load-bearing, and at least
two were got wrong once before being got right.

---

## Security decisions you should not casually reverse

### Over-length passwords are rejected before the user lookup, not before the rate limiter
`src/auth/login.rs`. The order is: `rate_limit::check`, `record_attempt`, over-length check,
user lookup, verify.

Rejecting an over-length password *after* branching on whether the user exists is a
user-enumeration oracle. But running the over-length check before `rate_limit::check` is worse:
it leaves the over-length path as the only route that still writes after a lock, letting an
attacker flood `login_attempts` and `audit_log` unthrottled. The real invariant is "before the
user lookup", not "first".

The cap is on **bytes** (256 chars x 4 = 1024), not `chars().count()`: a char count would reject
a legitimate long passphrase in a non-Latin script while still admitting a multi-megabyte ASCII
body. It must not call `burn_verify` — doing so would reintroduce the timing signal the check
exists to remove.

### Every wrong second factor is charged to the rate limiter before the code is verified
`src/auth/totp.rs`. This is the *only* thing bounding second-factor guessing. Successful
`/auth/login` attempts are flipped by `mark_succeeded`, so they count for nothing, and an
attacker holding the password can otherwise mint unlimited pending sessions. Do not move
`rate_limit::check` or `record_attempt` after verification, and do not charge only on the
restart path.

### Rate-limit keys are canonicalised before masking
`src/auth/rate_limit.rs::bucket`. IPv6 is masked to /64, because a single ordinary allocation
gives an attacker 2^64 addresses and /128 keying is therefore free to bypass. But an
IPv4-mapped address (`::ffff:a.b.c.d`, which a dual-stack listener or proxy may report) is
canonicalised to the IPv4 address it carries *first* — masking it would put every IPv4 client
on earth into one `::` bucket, which is a global sign-out denial of service strictly worse than
the bypass it was meant to fix. That regression shipped once in a fix round and was caught in
re-review.

### `authorize` returns 404 before it checks the role
`src/auth/mod.rs`. The `.ok_or(ApiError::NotFound)?` precedes `if role < min`, so no call site
can turn "not a member of this workspace" into a 403. This is structural, not a convention each
handler follows: all ten workspace-scoped handlers call `authorize` as their first statement
touching workspace data, nested statements carry `workspace_id` in the same predicate, and
`From<sqlx::Error>` maps `RowNotFound` to 404 so `fetch_one` paths land there too. The three
403s a cross-tenant caller can reach leak nothing they do not already know.

### The post-sign-in redirect is validated by parsing, not by pattern-matching
`web/src/lib/navigation.ts::nextPath`. This was got wrong twice.

First attempt: an allow-list over the decoded string. It rejected `//evil.example` and
`javascript:` but not `/%09/evil.example` — the WHATWG URL parser strips ASCII tab, CR and LF
*before* doing anything else, so a tab between the slashes survives a `startsWith` test and is
then deleted by the parser, producing a protocol-relative URL.

Second attempt: compare `new URL(next, origin).origin` to the current origin, then return
`target.pathname`. Also bypassable: a same-origin URL can carry a pathname that is itself
protocol-relative, so `/..//evil.example` passes the origin check and `location.assign` re-parses
the result as off-site. That payload names no host, so one crafted link works against every
deployment.

What works is a fixed point: parse, compare origins, then **re-parse the string you are about to
return** and accept it only if it still resolves to this origin. The second parse sees exactly
what `location.assign` will see. `web/e2e/next-path.spec.ts` pins twelve payloads; the
controller separately verified 24.

The lesson generalises: do not reason about which strings a parser rejects. Ask the parser what
it produces.

### `Cache-Control: no-store` is applied centrally, and must stay path-scoped
`src/http/security_headers.rs`. Without it, `GET /api/v1/me` returns JSON with no cache
directives at all; behind a shared cache — the deployment shape the spec assumes — one user's
response can be served to another, including their `csrf_token`, which is a write credential
rather than merely an identity. Every task that returned a secret had dutifully set `no-store`
on its own route; nobody owned the default.

It covers `/api*`, `/healthz` and `/readyz` (a cached readiness answer describes an earlier
instant, which is exactly what a probe must never report) and excludes `/assets/`, which keeps
`immutable`. `tests/http_foundation.rs` runs the real middleware over the real asset server and
would go red if the header were ever applied blanket — that test is the guard against a future
one-line "simplification" silently destroying asset caching.

### CSRF enforcement depends on layer ordering
`src/app.rs`. `require_csrf` wraps `http::routes()`, and axum's `.layer()` wraps only routes
added *before* it. A future `.merge()` placed after that call silently loses both CSRF
enforcement and the `Cookie` strip, together. There is a comment at the call site; keep it.

The bearer exemption is one predicate (`auth::bearer`) shared by the extractor and the
middleware. `require_csrf` returns early on *any* `Bearer` header, including a bare or malformed
one — safe only because the same middleware strips `Cookie` and the principal then fails closed
with `invalid-api-key`. That coupling is load-bearing.

### Recovery codes are salted
Unsalted SHA-256 over a 31^10 (~2^49.5) space is sweepable by a modest GPU rig in hours, and
unsalted means one sweep covers every user at once.

---

## Deliberate exceptions, so you do not "fix" them

- **`web_ui`'s 503 is `text/plain`**, not RFC 9457. It is the SPA fallback for a human in a
  browser who has not built the UI; problem+json would be worse for that reader. Every other
  error in the system is problem+json, including `/readyz`'s 503.
- **`/readyz`'s 200 must stay `{"database":"ok"}`.** `web/e2e/support/server.ts` matches that
  string to confirm the server it found on the port is ours and not some other service.
- **The API-key dialog is never axe-scanned.** An axe violation report embeds the offending
  node's HTML, so scanning a dialog that displays a one-time secret would write that secret into
  a test artifact. The gap is deliberate; do not close it by adding the scan.
- **`web/src/api/client.ts` uses `baseUrl: ""`.** The OpenAPI document has no `servers` entry and
  emits absolute `/api/v1/...` paths, so a `baseUrl` of `/api/v1` would request
  `/api/v1/api/v1/me`.
- **`uncacheable` and `is_reserved` are byte-identical but stay separate.** One means "not the
  SPA", the other "never cache". They coincide today; merging them would weld two ideas that
  merely happen to agree. A test asserts they agree, so divergence fails loudly.
- **`workspaces::for_user` orders by `name`; `workspaces::list` orders by `id desc`.** Not an
  oversight. `list` is cursor-paginated and its ordering is forced by the cursor; `for_user`
  feeds a switcher a human reads, and its first element drives the home redirect, which should
  not depend on creation order. The test fixture creates `mike`, `alpha`, `zulu` specifically so
  that name order, id-ascending and id-descending are three distinct sequences — do not tidy
  those names alphabetically.
- **RUSTSEC-2023-0071 is ignored in `deny.toml`** with a scoped justification. The Marvin attack
  targets RSA *private-key* operations; `rsa` reaches us only via `webauthn-rs` for RS256
  signature *verification*, and NM holds no RSA private key. Re-check on any `webauthn-rs` or
  `crypto-glue` bump — `crypto-glue` already ships private-key operations that a future version
  could start calling.

---

## Things that bit us, recorded so they do not bite again

- **The audit log's LEFT JOINs are unscoped.** They are safe only because the ids being joined
  came from rows already filtered by `where a.workspace_id = $1`. The day anything writes a
  foreign actor id into a workspace-scoped audit row, that join becomes a cross-tenant email
  disclosure. There is a comment; keep it.
- **`.sqlx/` force-nullable overrides are load-bearing.** Without the `?` on the audit join's
  actor columns, sqlx infers `String` and a deleted actor becomes a runtime `UnexpectedNull`.
- **`operationId` uniqueness is now asserted** (`src/openapi.rs`). utoipa derives ids from
  handler function names, so the published spec previously carried six operations called `list`.
  That is invalid OpenAPI, and openapi-typescript — which keys one `operations` interface by id
  — silently mistyped the generated client. `skipLibCheck` hid it. The symptom surfaced as
  unrelated frontend type errors.
- **The sessions sweep rides `resolve`'s once-a-minute `last_seen_at` write, not `create`.**
  Every `create` caller is inside a transaction, where a "best-effort" delete would abort the
  sign-in.
- **Run the full test suite as `cargo test -- --test-threads=4`.** The dev container reports 16
  CPUs but the WSL2 VM is capped near 3.9 GB, and each `#[sqlx::test]` costs an argon2 at
  m=19456 KiB plus its own database. Unthrottled, it killed the Docker engine three times. CI is
  unaffected.
- A full-suite run occasionally dies with `error reading contents of migration directory` — the
  Windows bind mount under concurrent compilation. Re-run once before investigating.

---

## Known gaps at the end of M0

- **No CI job has ever run on GitHub.** Nothing has been pushed. `pnpm/action-setup`, the pnpm
  cache, `playwright install --with-deps` and `cargo install sqlx-cli` are all unverified on a
  real runner, and the first push may need a fixup.
- **No e2e coverage** for passkey add/remove (needs a WebAuthn virtual authenticator, which is a
  task of its own), change-password, audit "Load more", the Copy buttons, or invite revoke.
- **The integration path for a built UI is never exercised by CI.** The `rust` job has no Node
  step and `web/dist` is gitignored, so the SPA fallback test always takes its 503 arm there. A
  unit test covers the same contract deterministically.
- **The Node tarball in `docker/dev.Dockerfile` is checked against nodejs.org's own
  `SHASUMS256.txt` over HTTPS.** That detects a corrupted download, not a compromised
  nodejs.org. Pinning the Node release GPG keys would close it.
- **The per-address cap on passkey sign-in challenges bounds one /64 or IPv4 address**, not a
  distributed botnet. Inherent to any per-address cap.
- **Reverse-proxy hop count is not configurable.** The rightmost-XFF single-trusted-hop
  behaviour assumes exactly one proxy. Record this in the deployment docs.
- **Disclosure change:** a workspace admin now sees the *email address* of any actor on their
  own workspace's audit rows, where before they saw a UUID. An instance admin who acted without
  being a member is named; a removed user keeps appearing by address until their `users` row is
  deleted. Nothing crosses a tenancy line — every row shown was written in that workspace — but
  it is a real widening from "an id" to "an identity", guarded only by `Role::Admin`. Deliberate;
  worth stating in the deployment docs.
