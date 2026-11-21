# M2 seam report — NM sending core

Read-only survey of `c:\Users\Utkarsh\Desktop\NEW MARKET`, branch `main`, tip `e1379bb`. Nothing
was changed, nothing was built. All line numbers are from that commit.

Scope note: nothing in `src/` sends mail. There is no SMTP client, no HTTP client, no worker, and
no Rust type for the `jobs` table. A grep for `jobs` across `src/*.rs` finds exactly one writer
(`src/public/forms.rs`) and three prose mentions (`src/forms/mod.rs:145`,
`src/suppressions/mod.rs:514`, `src/workspaces/mod.rs:242`). The milestone plan is in
`.scratch/email-platform/m2-decomposition.md` (M2a relays + worker, M2b templates, M2c campaigns
and pipeline, M2d tracking).

---

## 1. The `jobs` table and its claim protocol

### Columns

From `migrations/0023_forms_and_jobs.sql:125-135`, amended by `0025_jobs_completed_at.sql:38` and
`0026_jobs_references_and_consent_guards.sql:40-61`:

| column | type | notes |
|---|---|---|
| `id` | `uuid` primary key | UUIDv7 at the enqueue site, so it breaks `run_after` ties by enqueue order |
| `workspace_id` | `uuid not null` | `references workspaces(id) on delete cascade` |
| `kind` | `text not null` | deliberately **not** an enum and **not** under a check constraint: `0023` and `0026` both state that M2 adds kinds without a migration on this table |
| `payload` | `jsonb not null` | `check (jsonb_typeof(payload) = 'object')` (`jobs_payload_is_object`) |
| `run_after` | `timestamptz not null default now()` | earliest run time **and** the lease |
| `attempts` | `int not null default 0` | `check (attempts >= 0)` |
| `max_attempts` | `int not null default 5` | `check (max_attempts > 0)` |
| `last_error` | `text` | |
| `failed_at` | `timestamptz` | gave up; row kept |
| `created_at` | `timestamptz not null default now()` | what the resend throttle reads |
| `completed_at` | `timestamptz` | added by `0025`; success is an update, never a delete |
| `contact_id` | `uuid` | added by `0026`; composite FK `(workspace_id, contact_id) -> contacts on delete cascade` |
| `list_id` | `uuid` | added by `0026`; composite FK `(workspace_id, list_id) -> lists on delete cascade` |

Extra constraints from `0026`:

- `jobs_workspace_id_id_key unique (workspace_id, id)` — explicitly so a later table can take a
  composite foreign key onto a job. (The ADR notes that today "`jobs` is not a parent at all:
  nothing references it".)
- `jobs_confirmation_email_shape check (kind <> 'confirmation_email' or (contact_id is not null
  and list_id is not null))`. The only per-kind shape rule the schema enforces; a closed `kind`
  list was rejected on purpose.

Both `contact_id` and `list_id` are nullable because `kind` is open and a future kind need not be
about one contact. A composite FK with a NULL column is MATCH SIMPLE and is not checked.

### `kind` constraint

There is none. `kind` is a free `text` column. The only kind written today is
`'confirmation_email'`. M2 adds kinds by writing them.

### Payload shape

`0023` originally documented `{"contact_id", "list_id", "email", "token"}`. `0026` promoted the
two ids to columns, and the current payload for `confirmation_email` is:

```json
{"email": "…", "token": "<public confirm token string>"}
```

The `jobs` table comment (`0026`, lines 88-133) is the authority: "REFERENCES ARE COLUMNS, NOT
PAYLOAD. … Read them from the columns; a lookup must still be scoped by the row's `workspace_id`."

### The claim query — current contract

`0023:76-82` states a claim query that **`0026` supersedes**. `0023`'s version predates
`completed_at` and omits both `completed_at is null` and `attempts < max_attempts`; a worker
written from it re-claims every completed row the moment its lease expires, for ever. `0023`
cannot be edited (its checksum is taken by every database that has run it). The authority is the
`comment on table jobs` set in `0026:87-133`, which says so in those words. The current contract:

```sql
update jobs set attempts = attempts + 1, run_after = now() + <lease>
where id in (select id from jobs
             where run_after <= now() and failed_at is null and completed_at is null
               and attempts < max_attempts
             order by run_after, id
             for update skip locked
             limit $batch)
returning *;
```

Load-bearing, none optional, per the table comment:

- `for update skip locked` — without it two pollers claim the same row and both run it.
- `run_after` advanced in the **same statement**. It is the lease as well as the schedule.
- `completed_at is null` — from `0025`.
- `attempts < max_attempts` — a worker that dies after incrementing `attempts` and before writing
  `failed_at` otherwise retries the row past its limit for ever.

Terminal states:

- Success: `update jobs set completed_at = now() where id = $1`. **Never delete.**
- Giving up: set `failed_at` and `last_error`, leave the row.
- A `confirmation_email` row whose `created_at` is older than the `Purpose::Confirm` 14-day TTL
  carries a dead link: the comment says **fail it, do not mail it**. Nothing re-issues the token.

At-least-once delivery is the stated semantic. A worker that dies mid-job leaves the row claimed
until the lease expires; `attempts` has already counted the dead attempt. There is no unique
constraint to lean on, because the same job may legitimately be enqueued twice (a subscriber who
submits the form again before confirming).

### Retention trigger

`0026:135-170` — `jobs_throttle_retention()`, a `before delete … for each row` trigger. It raises
`'jobs rows are kept for the resend throttle: a row younger than a day may not be deleted'` when
`old.created_at > now() - interval '1 day'`. `pg_trigger_depth() > 1` admits referential actions
(the contact cascade that GDPR erasure needs, the list and workspace cascades). It is deliberately
**not** `enable always` — unlike the `consent_log` triggers in the same migration — because it
protects an operational decision made on the primary and must not stall a logical subscriber.

The one-day literal is a documented shortcut: `shortcut: the window is a literal, not derived from
RESEND_AFTER_MINUTES; raise it here if that constant ever exceeds a day.`

### Indexes

- `jobs_runnable on jobs (run_after, id) where failed_at is null and completed_at is null`
  (`0023:141`, recreated in `0025:43-45`). Carries the claim's own predicate and `order by`, so a
  terminal row leaves the index the moment it is marked.
- `jobs_confirmation_resend on jobs (workspace_id, contact_id, list_id, created_at desc) where
  kind = 'confirmation_email'` (`0026:78-80`). The resend throttle's equality probe. Partial, so
  rows of every kind M2 adds stay out of it.
- `jobs_pkey`, plus `jobs_workspace_id_id_key`.

Note what is **not** indexed: there is no index on `kind` alone, and the claim index is a single
global `(run_after, id)` ordering across every kind and every workspace. A FIFO drain is what the
schema is built for; per-kind or per-workspace fairness would need a new index.

### Who enqueues today

`src/public/forms.rs:576-617`, `enqueue_confirmation`, called from the public form submission path
(`apply`, line 224). It issues a `Purpose::Confirm` token with `kid: None, ref_id: None`, then:

```sql
insert into jobs (id, workspace_id, kind, contact_id, list_id, payload)
select $1, $2, 'confirmation_email', $4, $5, $3
 where not exists (select 1 from jobs
                    where workspace_id = $2 and kind = 'confirmation_email'
                      and contact_id = $4 and list_id = $5
                      and created_at > $6)
```

One row per `(contact, list)`: a `Confirm` token carries exactly one `list_id`
(`src/public/token.rs:148-150` refuses otherwise), so a form on two double-opt-in lists enqueues
two rows with two different tokens.

### The throttle, and why it constrains M2

`RESEND_AFTER_MINUTES = 15` (`src/public/forms.rs:515`). The throttle has **no state of its own**:
it counts `jobs` rows. `0025`'s whole argument is that a worker which deletes on success makes the
question answer "no" every time, turning an unauthenticated route into an open mailer, and
"**nothing goes red**". The predicate deliberately names no `completed_at` and no `failed_at`, so
narrowing it later is a visible decision. Measured cost on the real shape: 3-5 buffers, ~0.05 ms
with the index, versus 841 buffers and ~4 ms without it.

### Is the token in `payload` still a working confirm token?

**Yes.** `src/public/forms.rs:576-607` still writes the already-issued token string verbatim, and
`0026` re-decided to keep it rather than have M2 re-issue at send time. Three things make that
acceptable, and all three are now properties M2 must not break:

1. `jobs.contact_id` cascades from `contacts`, so GDPR erasure (`delete from contacts`) now
   destroys the credential with the person. Before `0026` it did not, and an erased person's row
   survived runnable.
2. A `Confirm` token is self-expiring (14 days), so a backup of this table goes inert.
3. It lets M1d prove end to end that a queued row yields a link `/c/{token}` accepts.

Practical consequence for M2: `jobs.payload` holds a live credential and a plaintext address. Do
not log payloads; do not copy a payload into another table that is not reached by the contact
cascade.

---

## 2. `AppState` and the router

### `AppState` — `src/app.rs:29-71`

```rust
pub struct AppState {
    pub pool: PgPool,
    pub config: Arc<Config>,
    pub secrets: Arc<Secrets>,
    pub webauthn: Option<Arc<Webauthn>>,
    pub preview_limit: Arc<RateLimiter>,
    pub selection_limit: Arc<RateLimiter>,
    pub export_limit: Arc<RateLimiter>,
    pub export_slots: Arc<Semaphore>,
    pub import_limit: Arc<RateLimiter>,
    pub public_limit: Arc<RateLimiter>,
    pub submit_limit: Arc<RateLimiter>,
    pub one_click_slots: Arc<Semaphore>,
}
```

Built by `AppState::new(pool, config)` (`src/app.rs:73-91`), which is also what `tests/common`
calls for every integration test (`tests/common/mod.rs:49`). Adding a field to `AppState` therefore
lands in every test binary.

`Config` (`src/config.rs:40-51`) is exactly: `database_url`, `listen`, `public_url`, `secret_key`,
`auto_migrate`, `trust_proxy_headers`, `metrics_listen`, `log_format`, `preview_timeout_ms`.
Loaded via `figment` from env/TOML. `public_url` is what every subscriber link must be built from.

### The router — `src/app.rs:100-120`

```rust
http::routes()
    .layer(middleware::from_fn_with_state(state.clone(), csrf::require_csrf))  // innermost
    .with_state(state)
    .layer(telemetry::metrics_layer())
    .layer(middleware::from_fn(security_headers))                             // outermost
```

Order matters and is commented: CSRF innermost so a 403 still gets security headers and is counted;
`security_headers` outermost so 404s and errors get the headers too.

**A `.merge(...)` added after the CSRF `.layer()` would silently get neither — no error, no failing
test.** Every route module merges inside `http::routes()` (`src/http/mod.rs:15-44`), which is also
where `crate::public::routes()` is merged and where the SPA `fallback` and the `/api/{*rest}`
catch-all live. M2's new modules go there.

`http::csrf::require_csrf` gates `/api/v1/` only, so public routes are CSRF-exempt **by
construction**, not by an exception list.

### Pool size and existing concurrency limits

- `src/db.rs:9-14` — `PgPoolOptions::new().max_connections(20)`. One pool for everything:
  HTTP handlers, `/readyz`, exports, imports, and whatever worker M2 adds.
- `contacts::export::MAX_CONCURRENT_EXPORTS` — instance-wide semaphore, 4 slots. An export holds a
  pooled connection for as long as the client takes to read.
- `public::unsubscribe::MAX_CONCURRENT_ONE_CLICKS = 4` (`src/public/unsubscribe.rs:71`) —
  instance-wide semaphore. Exists because `POST /u/{token}` may never answer 429, so the limiter
  cannot bound it; about twenty concurrent POSTs with one valid link held every connection in the
  pool and took `/readyz` and the whole authenticated API down. The slot is acquired **before** the
  connection (`src/public/unsubscribe.rs:666`).
- Five `RateLimiter`s, in-process fixed-window (`src/http/rate_limit.rs`). Documented shortcut:
  "the counter is per process, so N instances behind a load balancer allow N times the limit".

### What a worker needs from `AppState`

`pool` (its own connections, from the same 20), `secrets` (to issue tracking/unsubscribe tokens and
to compute suppression HMACs), `config.public_url` (to build links). Nothing else in `AppState`
today is useful to a worker, and everything else in it is request-shaped.

Two existing precedents for background work, both in `app::serve` (`src/app.rs:120-186`):

- **Awaited boot sweeps**: `contacts::import::routes::sweep_running_imports` and
  `sweep_orphaned_uploads`, awaited before the listener binds, best-effort (an error is a
  `tracing::warn!`, not a refusal to serve).
- **A spawned reaper**: `tokio::spawn(crate::contacts::selection::sweep_expired_snapshots(pool.clone()))`,
  started in `serve` and explicitly **not** in `AppState::new`, "which every test builds".

Shutdown is `shutdown_signal()` (SIGINT / SIGTERM) passed to `axum::serve(...).with_graceful_shutdown`,
then `pool.close()`. The spawned reaper is not awaited at shutdown; there is no task-tracker or
cancellation token in the codebase.

`cli.rs` has only `Serve`, `Keys`, `Admin` (plus an `openapi` output used by `pnpm gen:api`). There
is no `nm worker` subcommand today.

---

## 3. Secrets and crypto

### Key derivation

One master key: `Config::secret_key` → `SecretKey([u8; 32])` → `Secrets::new`
(`src/crypto/secrets.rs:57`). `Secrets` holds an `Aes256Gcm` cipher and a zeroized copy of the
master.

`Secrets::derive_key(&self, label: &str) -> zeroize::Zeroizing<[u8; 32]>`
(`src/crypto/secrets.rs:70`) = `hmac_sha256(master, label.as_bytes())`. It is `pub(crate)`.
"A distinct label gives an unrelated key, so a value keyed for one purpose can never be replayed as
another."

Primitives in `src/crypto/tokens.rs`: `random_token(bytes)` (line 10), `sha256` (17),
`hmac_sha256(key, msg)` (22), `constant_time_eq` (31).

### Domain-separation labels in use

| label | constant | used for |
|---|---|---|
| `nm:suppression:v1` | `crypto::SUPPRESSION_DOMAIN` (`src/crypto/mod.rs:23`) | `suppressions.email_hmac` |
| `nm:public-token:v1` | `public::token::PUBLIC_DOMAIN` (`src/public/token.rs:19`) | every subscriber link |

`SUPPRESSION_DOMAIN` is marked **ONE-WAY DOOR — never change this string**: an erasure tombstone has
no plaintext to recompute from, so changing it makes every erased person mailable again. Pinned by
`suppression_hmac_is_pinned` (`src/crypto/mod.rs:46`), whose expected hex was computed independently
with `openssl`.

`crypto::suppression_hmac(secrets, normalised_email) -> [u8; 32]` (`src/crypto/mod.rs:28`) is the
one way to compute a suppression hash. The address must already be through
`users::normalize_email` (trim, NFC, lowercase).

Separately, `Secrets::encrypt`/`decrypt` (lines 75, 99) is AES-256-GCM for secrets at rest, blob
layout `[version 0x01][key id 0x01][12-byte nonce][ciphertext||tag]` with the two header bytes as
AAD. M2 relay credentials are the obvious consumer.

### The public token format

`src/public/token.rs`. Wire format: `base64url_nopad(json_payload) "." base64url_nopad(hmac)`,
produced by `issue` (line 119) and verified by `parse` (line 134). The MAC is
`hmac_sha256(derive_key(PUBLIC_DOMAIN), payload_json_bytes)`, compared with `constant_time_eq`.
Signed, never encrypted. The same scheme as the selection token, "so there is one scheme to audit".

```rust
#[serde(deny_unknown_fields)]
pub struct Token {
    pub workspace_id: Uuid,
    pub contact_id: Uuid,
    pub purpose: Purpose,
    pub list_id: Option<Uuid>,   // Some only for Confirm
    pub issued_at: i64,          // unix seconds
    pub nonce: String,           // makes two links in one second differ
    #[serde(default, skip_serializing_if = "Option::is_none")] pub kid: Option<u8>,
    #[serde(default, skip_serializing_if = "Option::is_none")] pub ref_id: Option<Uuid>,
}
```

`Purpose` (line 36) has exactly two variants: `Confirm` and `Unsubscribe`. TTLs from
`Purpose::ttl_secs` (private, line 45): `Confirm` = 14 days, `Unsubscribe` = `None`, never expires.
There is deliberately **no** `Preference` variant; the comment at lines 26-33 explains why re-adding
one would be a mistake.

`parse(secrets, raw, expected)` returns `ApiError::Unprocessable("link-invalid")` for anything this
instance did not issue or a token for the wrong purpose, and `Unprocessable("link-expired")` for a
genuine link past its TTL. It also enforces `token.list_id.is_some() == (purpose == Confirm)`. The
expiry check runs **before any database access** — that is the anti-oracle property the confirm page
depends on.

### What M2 must reuse rather than reinvent

- **`kid: Option<u8>`** (line 101) — "which key signed this link". `None` means the master key this
  product has always had. It exists so rotating the master key does not invalidate every outstanding
  unsubscribe link, which spec §6.2 forbids. It was added by M1d *for M2*; M2 is the milestone that
  gives it a meaning. Note there is no key registry today: `derive_key` takes no key id and `Config`
  holds one `secret_key`.
- **`ref_id: Option<Uuid>`** (line 111) — "What sent this link: M2 fills it with a campaign or
  message id." Deliberately generic. The public routes are expected to copy it into
  `consent_log.source_ref`; without it that column is permanently null for every one-click
  unsubscribe M2 will ever send.
- The **frozen-field discipline** (lines 53-79). It is narrower than it looks: a later field added
  as `Option<T>` with `#[serde(default, skip_serializing_if = "Option::is_none")]` parses every link
  already sent. What `deny_unknown_fields` still refuses is an **older build reading a newer link** —
  which a rolling deploy produces for as long as both are serving, and which a test pins. Adding a
  `Purpose` variant has the same rolling-deploy shape.
- The rule that **anything a job needs travels beside the token, never inside it**
  (`0023`, `src/public/forms.rs:529-535`).

For M2's one-click unsubscribe: the token already exists, `/u/{token}` already implements RFC 8058
(`POST` with `List-Unsubscribe=One-Click`, 200 and an empty body per §3.2). M2's job is to put the
headers on the outgoing message and set `ref_id`, not to build a new endpoint.

---

## 4. The public routes and their contract

### What exists

`src/public/mod.rs`:

- `PREFIXES: [&str; 3] = ["/c", "/u", "/f"]` (line 20).
- `owns_path(path) -> bool` (line 42) — true for those prefixes and everything under them,
  **including the bare prefix with no token**, so `/u` and `/u/` 404 rather than falling through to
  the SPA. A near miss (`/contacts`, `/forms`) is not a match.
- `routes()` (line 50) merges `confirm::routes()`, `unsubscribe::routes()`, `forms::routes()`.

Routes: `GET /c/{token}` (`confirm.rs:28`), `GET|POST /u/{token}` (`unsubscribe.rs:37`),
`GET|POST /f/{workspace}/{slug}` (`forms.rs:52`). `{workspace}` is `workspaces.slug`.

`owns_path` is read by **both** `http::web_ui::is_reserved` ("never the SPA") and
`http::security_headers::uncacheable` ("never kept by a shared cache"), and a test
(`uncacheable_and_is_reserved_agree`, `src/http/security_headers.rs:79+`) pins that the two
predicates still coincide. **Adding `/t` for tracking is a one-line change to `PREFIXES` plus
whatever that test demands.**

### The invariants M2 inherits

1. **No session, ever.** `tests/public_pages.rs` walks the `src/public/` directory to enforce that
   nothing there reads the session. An operator's signed-in browser gets exactly the stranger's
   answer.
2. **No address oracle.** Spec §4's third anti-goal. Concretely:
   - `GET /c/{token}`: the expired page is reached **before any database access**
     (`confirm.rs:33-37`), so it is the same page whether or not the address exists. The expired
     page offers a re-send and deliberately gives no button to do it — a resend control reachable
     from an expired token would be an existence oracle. Do not "finish" it.
   - `POST /f/…`: **byte-identical responses** for new, existing and suppressed addresses — same
     status, same bytes, same headers. Pinned by
     `the_response_is_identical_for_new_existing_and_suppressed`
     (`tests/public_pages.rs:1924`), which also covers the second axis (a malformed optional field
     for each of the three). The one thing this route may answer differently is the **shape** of
     what was typed, because the submitter can see and fix it — and that check is run *before* the
     limiter and the honeypot are allowed to decide anything (`forms.rs:164-186`).
   - `POST /f/…` also **drops its `RateLimit-*` headers**, because they would be a "you have been
     counted" tell. The refused submission is answered with the success page and recorded only as
     `metrics::counter!("public_form_submissions_dropped_total")` plus a log line naming no address
     and no form (`forms.rs:194-206`).
   - A honeypot trip gets the success page too, and is deliberately **not** counted in that metric.
3. **`POST /u/{token}` one-click may never answer 429.** `apply` (`unsubscribe.rs:167-191`) reads
   the limiter verdict and ignores it for a one-click whose token **verifies** — the exemption is
   granted to a genuine link, not to a string in a body. The cost is bounded by
   `MAX_CONCURRENT_ONE_CLICKS` instead; a request without a slot waits, it is never shed.
   The `GET` (preference centre) **is** refusable with 429, because a person can retry.
4. **Every answer is HTML**, refusals included — never an RFC 9457 problem document. A subscriber
   who clicks a dead link gets a sentence.
5. **There is deliberately no shared "too many requests" page.** `page::with` is headers and nothing
   else (`page.rs:158-164`); each route writes its own refusal, because a shared helper is a refusal
   the two routes that must not refuse could inherit by reaching for the obvious name.

### Rate limiting

- `public_limit`: per **client IP** (bucketed to the /64 for IPv6 via `auth::rate_limit::bucket_of`),
  `PUBLIC_PER_MINUTE = 60` (`src/http/rate_limit.rs:175`), one shared budget across `/c/`, `/u/`,
  `/f/`. Its own map, so a flood fills it and cannot reach the four authenticated limiters.
- `submit_limit`: per IP, `SUBMIT_PER_MINUTE = 10` (line 207), spent **on top of** `public_limit`
  for `POST /f/…` only, and checked in two separate statements so neither becomes conditional on
  the other (`forms.rs:153-156`).
- `MAX_KEYS = 100_000` (line 250) bounds the limiter map's memory; once full, **every key not
  already in it is refused**, which is why the form's dropped-submission log line names that
  constant.
- `client_ip` (`src/http/request_meta.rs:30`) returns the **peer** address unless
  `trust_proxy_headers` is on. Behind a reverse proxy with the flag off, every public request keys
  on the proxy and the 61st person to click after a send gets a 429. `app::serve` warns at startup,
  but only when `public_url` is not localhost (`src/app.rs:141-166`).

### Security headers

`src/http/security_headers.rs:15-24`, set on **every** response by the outermost layer:

```
Content-Security-Policy: default-src 'self'; img-src 'self' data: blob:; style-src 'self' 'unsafe-inline';
                         frame-ancestors 'none'; base-uri 'none'; form-action 'self'
X-Frame-Options: DENY
X-Content-Type-Options: nosniff
Referrer-Policy: strict-origin-when-cross-origin
```

Plus `Cache-Control: no-store` for `uncacheable(path)` (line 50): the whole API, `/healthz`,
`/readyz`, and `public::owns_path`. It **overwrites** the header rather than deferring to the one
`page::shell` sets. Deliberately not the SPA or `/assets/`, which `web_ui` serves `immutable`.

No `script-src`, so scripts fall back to `default-src 'self'` and an inline script is blocked. Do
not add a script hash. The public pages carry no JavaScript at all and must work with it disabled.

### The rendering helper — `src/public/page.rs`

- `esc(s) -> String` (line 43). Hand-written, escapes `& < > " '`, implemented as a `match` so an
  entity it writes is never re-escaped. **Two documented shortcuts**: it does not make an *unquoted*
  attribute value safe (always write `title="{}"`), and it is **not a URL sanitiser** — a
  `javascript:` URL escapes to itself and still runs. `forms.redirect_url` is validated on the way
  in for exactly that reason (`src/forms/mod.rs`).
- `shell(title, heading, body) -> Response` (line 127). Escapes `title` and `heading`; **`body` is
  not escaped** — it is HTML the caller built, and every value it interpolated is the caller's to
  put through `esc` first. Emits `<!doctype html>`, `<meta name="robots" content="noindex">`, an
  inline `<style>` (the `STYLE` const, tokens copied from `web/src/styles/tokens.css` so the page
  needs no stylesheet request), and `Cache-Control: no-store`.
- `notice(status, title, body) -> Response` (line 152) — a page whose heading is its title.
- `with(headers, page) -> Response` (line 164) — attaches `RateLimit` headers.
- `UNKNOWN_IP` (line 147) — the bucket for a request whose client address is not visible.

No templating crate. `Cargo.toml` is curated and `check:licenses` runs over the tree; a dependency
for five substitutions is a dependency with an audit attached.

### Rules for M2's tracking endpoints

They are under the same public surface and must obey all of the above: no session, no address
oracle, HTML (or an image) and never a problem document, `no-store`, `nosniff` with a correct
content type, the prefix registered in `public::PREFIXES`, and a decision recorded about which
limiter they spend from. The decomposition's target (302 without waiting on the database, p99
< 20 ms at 5,000 req/s) sits in tension with `PUBLIC_PER_MINUTE = 60` per IP; see open questions.

---

## 5. The consent log and suppressions

### `consent::Action` — `src/consent/mod.rs:23-34`

`Requested`, `Subscribed`, `Confirmed`, `Unsubscribed`, `Resubscribed`, `Suppressed`. Stored as
text under `consent_log_action_check` (`0020`, widened by `0021`). `Requested` exists because a
`pending` membership is somebody who was *asked*, not somebody who agreed, and `lists.double_optin`
is mutable and unversioned — reading consent back through it would retroactively re-decide what an
old row meant.

### `consent::Source` — `src/consent/mod.rs:41-50`

`Form`, `Import`, `Api`, `Operator`, `OneClick`, `PreferenceCenter`, `Bounce`, `Complaint`.
**The code path, never the actor**: a session-authenticated operator using bulk subscribe is `Api`.
`Bounce` and `Complaint` have **no writer in `src/` today** — they are there for M2.

### What writes a consent row

- `consent::record(tx, workspace_id, contact_id, list_id, action, source, &Context)`
  (`src/consent/mod.rs:74`). Takes the **caller's connection** so it runs in the same transaction as
  the membership write it records. It scrubs its own `sqlx::Error` via `consent::scrub` (line 120),
  because Postgres copies the failing row — IP and user agent included — into a check violation's
  DETAIL and `ApiError::Internal` is logged with `{:?}`.
- `Context` (line 56): `ip`, `user_agent`, `source_ref`, `token_jti`. **No `Debug` impl, on
  purpose.** `token_jti` is the public token's nonce — *which* link was clicked. Operator and API
  paths must keep passing `None` for it: "a row naming a link the subscriber never followed reads as
  if they had."
- `Context::of_principal(principal)` (line 145) and `consent::principal_ref` (line 133) for operator
  paths.
- **There is deliberately no `record_many`** (line 104). Set-based writers record in the same
  statement that wrote the rows, as a `logged` CTE over that statement's `returning` — see
  `contacts::selection::run_recorded` and the importer's `write_memberships_and_tags`. A batch
  helper could not express per-row actions and would cost a round trip per row on a million-row
  import. **M2's campaign launch, which writes per-recipient rows, should follow that CTE pattern,
  not reach for a batch helper.**

### Append-only enforcement

`consent_log_no_update` (`0020:58`), `consent_log_no_delete` and `consent_log_no_truncate`
(`0026:195-200`), all three `enable always` (`0026:216-218`) so the guarantee survives
`session_replication_role = 'replica'`. `pg_trigger_depth() > 1` admits the one legitimate update
(`on delete set null (list_id)` from deleting a list) and the one legitimate delete (the contact
cascade from erasure). The documented cost: on a logical subscriber those changes arrive at depth 1
and are refused, stalling replication — deliberate, so turning the guarantee off is a recorded act.

M2 cannot write a consent row and later correct it. Get it right in the transaction.

### `suppressions::Reason` and `Scope` — `src/suppressions/mod.rs:29, 42`

Rust: `Reason::{UnsubscribedAll, Bounced, Complained, Manual, GdprErasure}`;
`Scope::{Address, Domain}`.

Database (after `0027_list_scope_suppressions.sql`): `scope in ('address','domain','list')`,
`reason in ('unsubscribed_all','bounced','complained','manual','gdpr_erasure','unsubscribed_list')`,
with `check ((scope = 'list') = (reason = 'unsubscribed_list'))`.

**The Rust enums deliberately have no variant for `list` / `unsubscribed_list`**, so an
address-level query that reaches one of those rows fails to decode and the request errors, rather
than reporting a workspace-wide block that is not there. The admin API reads the
`address_suppressions` view (`0027:201`), which is `select * from suppressions where scope in
('address','domain')`.

### Send-time eligibility — what to consult

The one definition is the view `mailable_list_members`, created in
`migrations/0015_mailable_view.sql:17` and replaced in `migrations/0027_list_scope_suppressions.sql:210`.
`0015`'s header says it in as many words: "the mailable count uses it now; **the send-time recipient
resolution must use it later, never a fresh copy of the rule**."

Current definition — a `(workspace_id, list_id, contact_id)` is mailable when:

1. `list_memberships.status = 'subscribed'` (so `pending` and `unsubscribed` are out), joined to a
   live `contacts` row in the same workspace; **and** none of these five exists:
2. a live `address`-scope suppression in **this workspace** matching `contacts.email`;
3. a live `address`-scope suppression **instance-wide** (`workspace_id is null`) matching `contacts.email`;
4. a live `domain`-scope suppression in this workspace matching `contacts.email_domain`;
5. a live `domain`-scope suppression instance-wide matching `contacts.email_domain`;
6. a live `list`-scope suppression in this workspace with `s.email = c.email and s.list_id = m.list_id`.

"Live" is `lifted_at is null` throughout. Clause 6 is one clause and not a pair, because a list
belongs to exactly one workspace and there is no instance-wide list.

Two things it is written as it is on purpose, both re-stated in `0027:204-209`:

- **Five separate `not exists` clauses, never `or`-merged.** An OR-qualified anti-join cannot use
  the partial indexes and degrades to a sequential scan — measured. "Ugly, measured, deliberate: do
  not merge them."
- **It matches on plaintext `email`, not the HMAC.** Erasure tombstones carry `email is null` and so
  match nothing here, which is correct — an erased contact has no `contacts` row to count. The
  single-address path is `suppressions::is_suppressed`, which matches on the HMAC.

Related single-address helpers:

- `blocking_suppression(db, secrets, workspace_id, email) -> Option<Block>`
  (`src/suppressions/mod.rs:118`). Most-specific block: workspace before instance, address before
  domain. Its doc comment says explicitly "It is for the **send-time check** (a later milestone) and
  for explaining a contact's page, NOT for creating or importing a contact." An address that cannot
  be normalised is a 500, never `None` — it fails closed. A domain block is **exact**: it does not
  cover subdomains, and "a send-time check must not assume it does."
- `is_suppressed(...) -> bool` (line 134) — same predicate, boolean.
- `is_suppressed_excluding_bounce(...)` (line 166) — "is there a block here that is **not** a
  bounce", asked of every row at once. It is **not** `blocking_suppression` with a reason test on
  the answer: a workspace-scope `bounced` row would otherwise mask an instance-wide complaint. Used
  by `public::forms`.
- `is_erased(...)` (line 286), `erase_address(...)` (line 323), `lock_address_for(db, secrets, email)`
  (line 270), `unsubscribe_all` (384), `lift_unsubscribed_all` (439), `unsubscribe_list` (527),
  `lift_unsubscribed_list` (605), `suppressed_lists` (656).

`GET /contacts/{id}/eligibility` (`src/contacts/routes.rs:222`) returns
`Eligibility { mailable: bool, blocked_by: Option<Block> }`, computed server-side because the answer
can depend on an instance-wide block a workspace's admins cannot read. `GET /lists/{id}/mailable-count`
(`src/lists/routes.rs:509`) counts over the view — 1.4 s warm over 1.43M mailable contacts, so it is
deliberately exempt from the 150 ms p95 target.

---

## 6. ADR 0005 — one lock order for every path

`docs/adr/0005-one-lock-order-for-every-path.md`, accepted 2026-11-21.

### Why it exists

M1d shipped four ABBA deadlocks in three review rounds. Each round fixed the pair it was told about
and argued that pair was now safe; the next round found another. Nine paths take more than one of
these locks, so there are thirty-six pairs, and each new writer re-derived its own order from
whichever neighbouring comment it read. "That is a defect generator, not a defect" — and none of it
is reachable by the test suite, which was green through all four.

### The order

```
1.  the address advisory lock   suppressions::lock_address / lock_address_for
2.  lists rows                  lists::lock_lists (for key share)
2a. form_lists rows             (below lists; unordered against 3-6)
3.  suppressions rows
4.  contacts rows               contacts::lock_contact (for key share)
5.  list_memberships rows
6.  consent_log rows
```

Within a class, by `id` ascending; `lists::lock_lists` sorts for you.

Why each position is forced: `lists` cannot move below `suppressions` (the `0027` rename trigger
runs `lists` → `suppressions` inside one statement) nor below `list_memberships` (`delete_list`
locks the list row before anything cascades); `suppressions` must sit above `list_memberships`
(`erase_contact` writes the tombstone before deleting the contact); `contacts` sits below
`suppressions` and above `list_memberships` for the same erasure reason; `consent_log` is last
because it records what the membership write did; the advisory lock is first because it is the only
non-row lock and it serialises two transactions about one address before either reads anything.

**The trap worth internalising:** every writer of `list_memberships` or `consent_log` takes a
`lists` row *implicitly* through its foreign key, and a foreign key check runs at the **end** of the
statement. So the natural order of every membership writer is `list_memberships` → `lists`, which is
backwards. Each such path now takes the `lists` rows it is about to touch **explicitly, first**.

### The path table

| path | order it takes |
|---|---|
| `public::unsubscribe::save` (preference centre) | A, L (every candidate list), S, contacts, M, C |
| `public::unsubscribe::one_click` | A, L (the contact's lists), S, contacts, M, C |
| `public::forms::apply` | A, L (the form's lists), S (read), contacts, M, C |
| `public::confirm` | L, contacts, M, C |
| `contacts::routes::erase_contact` | A, S, contacts, M, C (cascade). No `lists` row. |
| `contacts::routes::delete_contact` | contacts, M, C (cascade). No tombstone, so no S. |
| `contacts::selection::delete` | contacts, M, C (cascade), over a selection |
| `contacts::import::staging::upsert_from_staging` | L (job's list + every list the file names), contacts (exclusive upsert), M, C |
| `contacts::selection::subscribe` / `unsubscribe` | L, contacts (the `sel` CTE), M, C |
| `lists::add_member` / `unsubscribe_member` | L, contacts, M, C |
| `lists::routes::delete_list` | L, S, M, C (cascade) |
| `lists::routes::update_list` (rename) | L, S (trigger) |
| `forms::routes::create_form` / `update_form` | the `forms` row, L (`set_lists`), `form_lists` |
| `contacts::import::routes::start_import` | L (held, not merely read), then `imports` |
| `contacts::selection::tag` | the `tags` row (`for key share`), `contact_tags` |
| `suppressions::routes::create_suppression` / `lift_suppression` | S, C (through `record_workspace_optout`) |

### The rule for new code — verbatim obligations

> If you write `list_memberships` or `consent_log`, call `lists::lock_lists` with the list ids you
> are about to name and `contacts::lock_contact` with the contact, in that order, first. If you also
> touch `suppressions`, do that between the two. If the write is about one address, take
> `suppressions::lock_address_for` before any of it.
>
> **Use what `lock_lists` returns.** A caller that inserts must narrow itself to the ids that came
> back; one that only updates may discard it. Getting that wrong is a foreign key violation reported
> as a 500.
>
> If a path cannot obey this, put it in the table with the reason.

Signatures:

- `lists::lock_lists(db: &mut PgConnection, workspace_id: Uuid, ids: &[Uuid]) -> sqlx::Result<Vec<Uuid>>`
  (`src/lists/mod.rs:178`) — `... where workspace_id = $1 and id = any($2) order by id for key share`.
- `contacts::lock_contact(db: &mut PgConnection, workspace_id: Uuid, id: Uuid) -> sqlx::Result<bool>`
  (`src/contacts/mod.rs:317`) — `select 1 ... for key share`.
- `suppressions::lock_address_for(db: &mut PgConnection, secrets: &Secrets, email: &str) -> sqlx::Result<()>`
  (`src/suppressions/mod.rs:270`) — normalises, hashes, `pg_advisory_xact_lock`.

### The closed enumeration and the two live tripwires

Nine foreign-key parents, all ruled on: `lists` (class 2), `contacts` (class 4), and seven that are
not classes (`forms`, `tags`, `imports`, `selection_snapshots`, `api_keys`, `workspaces`, `users`).
The method: *a class exists wherever one path writes a child table directly and another path reaches
the same rows through a parent's `on delete` action.* **"If a tenth parent appears, it is a new row
in that table."** M2 adds parents (`campaigns`, `templates`, `relays`, `sending_domains`,
`messages`) — each one is a ruling that must be written into this table.

Two live tripwires M2 should know about:

- **`tags` becomes class 2b** the day a `PUT /contacts/{id}/tags` (delete-and-reinsert over
  `contact_tags`) is written; it then takes `for key share` on the tag rows first.
- **Known and open: the importer's tag race.** `write_memberships_and_tags` still raises `23503` on
  `contact_tags_workspace_id_tag_id_fkey` when a tag is deleted while it runs, failing the whole
  import. Pre-existing, reproduced, recorded.

Also measured and worth re-checking on a Postgres major upgrade: `FOR KEY SHARE` being granted past
an already-queued `FOR UPDATE` waiter is an implementation property of `heap_lock_tuple`, not a
documented guarantee (measured on 16.15).

`jobs` is currently a **child** of `workspaces`, `contacts` and `lists`, and a parent of nothing.
A worker that claims a job and then writes `list_memberships` / `consent_log` is a new path for this
table: it must take A (if the write is about one address), then L, then S, then contacts, then M,
then C — exactly like `public::confirm` does.

---

## 7. Conventions a new module must follow

### Error type and HTTP mapping — `src/error.rs`

`ApiError` (line 34) variants and statuses: `Validation(Vec<FieldError>)` → 422 `validation` with
`errors: [{field, message}]`; `Unauthorized(&'static str)` / `UnauthorizedDetail` → 401;
`Forbidden` → 403; `NotFound` → 404 `not-found` (also the answer for another workspace's
resources); `Conflict` / `ConflictDetail` → 409; `Unprocessable` / `UnprocessableDetail` → 422;
`PayloadTooLarge` → 413; `TooManyRequests { retry_after_secs }` → 429 + `Retry-After`;
`UnsupportedMediaType` → 415; `ServiceUnavailable` → 503; `Internal(anyhow::Error)` → 500 with
detail always `internal error` and the cause only logged.

Body is RFC 9457 `application/problem+json` with `type = urn:nm:problem:<code>`.
`type ApiResult<T> = Result<T, ApiError>`. `From<sqlx::Error>` maps `RowNotFound` → 404 (so a
`fetch_one` scoped by `workspace_id` on another workspace's id answers not-found); use
`fetch_optional` to decide otherwise. `From<anyhow::Error>` → `Internal`.

### Pagination — `src/pagination/mod.rs`

`PageParams { limit: Option<u32>, cursor: Option<String> }` (line 19), `DEFAULT_LIMIT = 50`,
`MAX_LIMIT = 200`. Queries `order by id desc`, filter `id < after()`, fetch `limit() + 1`.
`Page<T> { data, next_cursor }` (line 60) built by `Page::from_rows(rows, limit, |r| r.id)` or
`Page::from_numbered_rows` for row-number-keyed lists. Cursors are base64url-nopad: 16 bytes for a
uuid, 8 for a row number, so a cursor minted for the other list kind is refused rather than silently
restarting. A malformed cursor is a 422 `validation` on field `cursor`.
`pagination::sorted` carries `ContactSort` / `Dir` for the sorted contact list.

### OpenAPI registration — `src/openapi.rs`

One `#[derive(OpenApi)] struct ApiDoc` with a `paths(...)` list (every handler, line 13-105) and a
`components(schemas(...))` list (every type, line 106-201). A new module adds its handlers to the
first and its types to the second. Routes are served at `/api/v1/openapi.json` and `/api/v1/docs`
(Swagger UI, vendored, validator badge disabled so no third-party call is made).

Two guards, both tests:

- `operation_ids_are_unique` (line 239). utoipa derives `operationId` from the **function name**, so
  two modules each exporting a `list` silently ship the same id; the damage lands in the generated
  TypeScript, where one `operations` interface is keyed by that id. Fix with an explicit
  `operation_id = "…"` in `#[utoipa::path(…)]` — which is why you see `operation_id = "mailable_count"`
  and friends throughout.
- **The schema-name collision guard**, `schema_names_are_unique` (line 283). utoipa derives a
  schema's name from the Rust type's last path segment and `components.schemas` is one flat map
  keyed by it, so two registered types sharing a name (`consent::Action` and a future
  `audit::Action`) **do not collide loudly** — the second insert replaces the first, every `$ref`
  still resolves, and one type ships to every client with the other's shape. The test therefore
  reads the **registration list in its own source** (`include_str!("openapi.rs")`), splitting on
  **commas, not newlines** (a past bug: two paths on one line collapsed into one entry and the
  collision shipped), cutting `//` comments, and cross-checking that every derived name is a key of
  the published document. Fix a collision with `#[schema(as = "…")]`.
  M2 must watch for this: `Template`, `Message`, `Status`, `Campaign`, `Domain` are all plausible
  double-registrations.

### SPDX headers

Every source file starts with `// SPDX-License-Identifier: AGPL-3.0-only` (or `-- ` / `<!-- -->`).
`scripts/check-spdx.sh` fails if any `.rs .sql .ts .tsx .mjs .html .css .js` under
`src migrations tests web/src web/scripts web/e2e web/public web/index.html` lacks it in the first
three lines.

**This collides with a Postgres capability and the collision is documented.** `0026:233-240`:
`src/db.rs` runs `sqlx::migrate!`, and sqlx 0.9.0 treats a migration as non-transactional only when
the file *starts with* `-- no-transaction`. Every migration here opens with the SPDX header, so
**every migration runs inside a transaction**: `create index concurrently` cannot run at all, and
the `drop / add not valid / validate` dance buys nothing. Using either needs a carve-out in
`check-spdx.sh` that nobody has made. `issues/24` carries the long version.

### sqlx offline mode

`.sqlx/` holds 172 `query-*.json` files, checked in. CI sets `SQLX_OFFLINE: "true"`
(`.github/workflows/ci.yml:26, 167`) and runs, in order: `sqlx migrate run`, `cargo fmt --check`,
`cargo clippy --all-targets -- -D warnings`, `cargo test`, then
`cargo sqlx prepare --check -- --all-targets`. sqlx-cli is pinned to 0.9.0, equal to `sqlx` in
`Cargo.toml`. `build.rs` emits `cargo:rerun-if-changed=migrations` so a new migration file triggers
a rebuild of the embedded migrator.

Any new `sqlx::query!` needs a regenerated `.sqlx` entry or CI's `prepare --check` fails.

### Tests

Flat `tests/*.rs` integration binaries (34 of them), each `mod common;` over `tests/common/mod.rs`.
`#[sqlx::test]` per test gives a fresh migrated database and a `PgPool`; `tests/common::TestApp::new(pool)`
builds `AppState` with `test_config` (`secret_key: SecretKey([7u8; 32])`, `auto_migrate: false`,
`trust_proxy_headers: false`) and an `axum_test::TestServer` built with
`into_make_service_with_connect_info::<SocketAddr>()` so handlers see `ConnectInfo` as in production
(pinned by `tests/harness.rs`). `TestApp::login` reads the CSRF token from `/me` and sends it on
later unsafe methods.

Notable existing suites M2 will touch or extend: `public_pages.rs` (2,992 lines — the invariant
suite, and the file that walks `src/public/` to forbid session reads), `lock_order.rs` (1,219 lines),
`eligibility.rs`, `consent.rs`, `suppressions.rs`, `erasure.rs`, `forms.rs`.

### The dev container

`bash scripts/dev.sh <command>` runs anything in the Linux dev container:
`docker compose -f docker-compose.dev.yml -p nm run --rm -e CARGO_BUILD_JOBS=${CARGO_BUILD_JOBS:-2} dev "$@"`.
The job cap is deliberate: the WSL2 VM is capped at 4 GB and `cargo test` links 13 integration
binaries at once; unthrottled it took the whole Docker engine down three times.
`scripts/dev-serve.sh` is the serving variant. `MSYS_NO_PATHCONV=1` stops Git Bash mangling paths.

### What the web app expects, and how the contract is generated

`web/src/api/` holds three files: `client.ts` (hand-written), `openapi.json` and `schema.d.ts`
(both generated, both committed).

```
pnpm gen:api  = cargo run -q -- openapi > src/api/openapi.json && pnpm gen:schema
pnpm gen:schema = openapi-typescript src/api/openapi.json -o src/api/schema.d.ts   (then prepends the SPDX line)
```

CI diffs the regenerated spec against the committed one (`ci.yml:66-90`), so an API change that is
not regenerated fails the build.

`client.ts` wraps `openapi-fetch` with:
- a CSRF middleware for `POST PUT PATCH DELETE`, token set by `setCsrfToken` after reading `/me`;
- `problemDetail`, `problemMessage`, `problemType`, `problemText(error, known, fallback)` — the one
  place the "generated types say error responses have no content, but the body is really a problem
  document" gap is bridged. `problemText` maps a problem **code** (`tag-name-taken`) to wording
  written for the screen, "because the server's own `detail` is only the code with its dashes
  removed". **So every new `ApiError::Conflict("…")` code M2 adds is a string the frontend keys
  off.**
- a 401-means-no-session redirect to `/login`.

Stack: React 19, TanStack Router + Query, Tailwind 4, Vite 6, Playwright for e2e. The app shell is
capped at 150 KB compressed (`pnpm check:bundle`), and the decomposition names the email editor as
one of two things that must be loaded on demand rather than put in the shell.

---

## 8. What already exists that M2 might wrongly duplicate

| M2 need | What exists | Where |
|---|---|---|
| **Templating / HTML escaping** | `page::esc` and `page::shell`. No templating crate anywhere, deliberately. `esc` is **not** a URL sanitiser and does not cover unquoted attributes. | `src/public/page.rs:43, 127` |
| **URL validation** | `forms.redirect_url` is validated on the way **in** — absolute http/https only — precisely because `esc` cannot make a `javascript:` URL safe. Reuse that rule for tracked links. | `src/forms/mod.rs`, `0023` header |
| **Link tokens** | The whole public-token scheme, with `kid` and `ref_id` already reserved for M2. Do not invent a second signed-link format. | `src/public/token.rs` |
| **One-click unsubscribe** | Already implemented and already RFC 8058-correct, including the never-refuse rule and the semaphore that bounds it. M2 adds headers to outgoing mail; it does not add an endpoint. | `src/public/unsubscribe.rs:167, 660` |
| **Suppression / eligibility** | `mailable_list_members` is the single definition; `blocking_suppression`, `is_suppressed`, `is_suppressed_excluding_bounce` are the single-address paths. Do not write a fresh copy of the rule. | `0015`, `0027:210`, `src/suppressions/mod.rs:118,134,166` |
| **Consent recording** | `consent::record` + `Context`, and the "record in the same statement via a `logged` CTE" pattern for set-based writers. There is deliberately no batch helper. | `src/consent/mod.rs:74,104` |
| **Rate limiting** | `http::rate_limit::RateLimiter` — in-memory fixed window, keyed by `Uuid` or `IpAddr` (/64-bucketed), with `MAX_KEYS` eviction and `RateLimit-*` header emission. Per **process**, documented shortcut. Also `auth::rate_limit`, the database-backed login limiter that survives restarts and is shared across instances. | `src/http/rate_limit.rs`, `src/auth/rate_limit.rs` |
| **Concurrency ceilings** | Two `Arc<Semaphore>` precedents sized against the 20-connection pool: `MAX_CONCURRENT_EXPORTS = 4`, `MAX_CONCURRENT_ONE_CLICKS = 4`, both acquired before a connection. | `src/contacts/export.rs`, `src/public/unsubscribe.rs:71` |
| **Batching / streaming large sets** | The importer streams an `AsyncRead` straight into `COPY` via `csv-core` (no blocking reader), stages into a temp table, and does the membership + tag + consent write as one set-based statement. The exporter streams with a `SEND_TIMEOUT` per chunk. `contacts::selection` materialises a selection as a snapshot with its own reaper. | `src/contacts/import/staging.rs`, `src/contacts/export.rs`, `src/contacts/selection.rs` |
| **Background sweeps / "worker-ish" code** | Boot sweeps awaited in `serve` (`sweep_running_imports`, `sweep_orphaned_uploads`) and one `tokio::spawn`ed reaper (`sweep_expired_snapshots`). Best-effort: an error warns, it does not refuse to serve. | `src/app.rs:120-186` |
| **Retry / backoff** | **Nothing.** The only `retry` in `src/` is `ApiError::TooManyRequests { retry_after_secs }` and the importer's "a failed run rolls back to `pending`, and `?` is the retry". `jobs.attempts` / `max_attempts` / `run_after` are the retry primitives and they have no Rust code behind them. | — |
| **Scheduling** | **Nothing**, beyond `jobs.run_after`. No cron, no scheduler, no timer wheel. | — |
| **Timezone handling** | `chrono-tz 0.10.4` is already a dependency. `workspaces::validate_timezone` validates an IANA name; `contacts.timezone` is a validated `text` column, settable via API and importable from CSV. No code converts between zones. | `src/workspaces/mod.rs`, `src/contacts/mod.rs:40,216` |
| **Metrics** | `telemetry::metrics_layer()` labels by method, **matched route template** (never the raw path, to bound cardinality; unrouted = `route="unmatched"`) and status. Prometheus exporter on `metrics_listen`. One bespoke counter exists: `public_form_submissions_dropped_total`. | `src/telemetry.rs:42,77-87` |
| **Secrets at rest** | `Secrets::encrypt`/`decrypt`, AES-256-GCM with a versioned, key-id'd, AAD-authenticated blob header. Use it for relay passwords and DKIM private keys. | `src/crypto/secrets.rs:75,99` |
| **SMTP / HTTP client** | **Nothing.** No `lettre`, no `reqwest`. `rustls` is already in-tree via sqlx's `tls-rustls-ring-native-roots`. `deny.toml` and `pnpm check:licenses` gate new dependencies. | `Cargo.toml` |
| **MJML / mrml** | Not a dependency. | — |

---

## Open questions — where the codebase does not decide

**Worker shape and deployment**

1. **Lease length.** The claim statement requires `run_after = now() + <lease>`; no constant, no
   config key, no comment names a value. It has to cover the slowest SMTP handshake plus retries.
2. **Batch size.** `limit $batch` is likewise unspecified.
3. **Poll vs. notify.** Nothing in the schema or the docs says whether the worker polls
   `jobs_runnable` on an interval or uses `LISTEN`/`NOTIFY`. Nothing emits a `NOTIFY` today.
4. **Where the worker runs.** In-process `tokio::spawn` inside `nm serve` (the
   `sweep_expired_snapshots` precedent) or a separate `nm worker` subcommand? `cli.rs` has only
   `Serve`, `Keys`, `Admin`. If in-process, the worker's connections come out of the same
   `max_connections(20)` as `/readyz` and the API — the exact failure mode `MAX_CONCURRENT_ONE_CLICKS`
   was invented for. Nobody has decided whether the pool grows, splits, or gets a third semaphore.
5. **Shutdown and in-flight leases.** `shutdown_signal` covers axum only; `pool.close()` runs right
   after. A worker mid-send on SIGTERM either loses its connection or must be awaited — there is no
   task tracker or cancellation token anywhere in the tree.
6. **Multi-instance.** Every limiter is per process (documented shortcut). The decomposition wants
   "per-relay rate limiting (token bucket **shared across workers**)". There is no shared store, no
   Redis, no advisory-lock-based bucket. Whether M2 introduces one, or declares single-instance, is
   open.
7. **Fairness.** The claim is strict FIFO on `(run_after, id)` across every kind and every workspace,
   and `jobs_runnable` is the only index that serves it. A million-row campaign enqueued as jobs
   would sit in front of every confirmation email in the instance. Whether campaign work goes in
   `jobs` at all, or gets a `priority` column (which is a migration on a then-large table), or is
   claimed by a separate query over `messages`, is undecided.
8. **`jobs` vs. `messages`.** The decomposition wants a `messages` table, one row per recipient,
   unique on `(campaign_id, contact_id)`. Its relationship to `jobs` is unspecified: is a job a
   batch of messages, one message, or a campaign-level tick? `0026` added
   `jobs_workspace_id_id_key unique (workspace_id, id)` specifically so a child could reference a
   job, but nothing does.
9. **Retention housekeeping.** Nothing deletes completed jobs. The trigger only forbids deleting
   rows younger than a day. Who runs the sweep, how often, and against what age is open — and so is
   whether the sweep must then respect the `messages` table's own history requirements.
10. **Stale-confirmation detection.** The contract says a `confirmation_email` row older than the
    `Confirm` TTL must be failed, not mailed. `Purpose::ttl_secs` is **private**
    (`src/public/token.rs:45`), so the worker either gets it made `pub` or duplicates `14 * DAY`.
    Duplicating it is exactly the "fresh copy of the rule" that `0015` forbids elsewhere.

**Tokens and crypto**

11. **`kid` semantics.** The field is `Option<u8>` and `None` means "the master key this product has
    always had". There is no key registry: `derive_key` takes no key id and `Config` holds exactly
    one `secret_key`. What `kid: Some(1)` resolves to, where a second key is configured, and how
    `Secrets` is extended to hold several, is entirely open.
12. **`ref_id` is one `Uuid`.** Campaign id, or message id? It cannot be both, and the field list
    cannot be widened cheaply. Picking message id makes per-link attribution exact but makes
    `consent_log.source_ref` name something an operator cannot look up without a join; picking
    campaign id loses which message.
13. **Tracking-token format.** No `Purpose` variant exists for clicks or opens, and the target URL
    is not a `Token` field. Three sub-questions, all open: (a) add `Purpose::Click`/`Open` plus a
    URL-carrying field, accepting that an older build in a rolling deploy rejects the new links via
    `deny_unknown_fields`; (b) store the link in a table and put only its id in the token, which
    costs the database lookup the p99 target says to avoid; (c) a separate, shorter token scheme
    with its own domain label, which breaks the "one scheme to audit" property. The payload is
    base64url'd JSON containing two UUIDs and a nonce, so a tracked link is already long before any
    of this.
14. **Does an open pixel need a token with a `contact_id` in it at all?** Nothing decides whether
    opens are attributed per recipient (and so leak a contact id into every forwarded email) or per
    message.

**Public surface**

15. **Rate limiting for `/t/`.** `PUBLIC_PER_MINUTE = 60` per IP, with a shared window behind a
    proxy, is far below what click traffic and scanner fleets produce after a send. Whether tracking
    spends from `public_limit`, gets its own limiter, or is unlimited and bounded some other way is
    undecided — and "may never answer 429" has precedent (`one_click`) with a cost attached
    (a semaphore).
16. **Host-aware routing.** Per-workspace tracking domains (CNAME) mean requests arrive on a host
    that is not `public_url`. Nothing in the router looks at `Host` today, and `app::serve` binds one
    listener. How the on-demand-TLS "is this domain allowed" endpoint is authenticated to the
    bundled Caddy is also open.
17. **CSP and the pixel.** `img-src 'self' data: blob:` means the SPA cannot load an image from a
    tracking domain. Mail clients ignore our CSP, so delivery is fine, but any in-app preview or
    analytics thumbnail served from a tracking domain needs the header widened — a decision with a
    documented "do not widen casually" comment attached.
18. **Does `/t/` belong in `owns_path`?** Adding it forces `no-store` **and** "never the SPA", and
    the paired test demands both predicates agree. `no-store` on a tracking pixel is probably right;
    nobody has written it down.

**Eligibility, bounces, and consent**

19. **Which eligibility check runs at send time.** The view is the declared definition, but it
    matches on plaintext `email` and so cannot see erasure tombstones, while `is_suppressed` matches
    on the HMAC and is per-address. The decomposition says "Suppression re-checked at send time, one
    lookup per batch". A batch lookup over the view, a batch lookup over `suppressions` by HMAC, and
    a per-address `blocking_suppression` are three different answers with three different costs, and
    only the per-address one sees a tombstone.
20. **Does a `bounced` suppression stop a campaign?** `blocking_suppression` finds every reason
    including `bounced`; `is_suppressed_excluding_bounce` exists precisely because the forms path
    wanted the other answer. Which one the send pipeline asks is undecided, and the masking problem
    documented at `src/suppressions/mod.rs:166-190` is a live trap either way.
21. **What a bounce writes.** `Source::Bounce` and `Source::Complaint` exist with no writer;
    `Reason::Bounced` and `Reason::Complained` exist with only the admin API able to create them.
    Open: does a bounce write a consent row per list or one workspace-wide row with `list_id = null`?
    What `Action` — `Suppressed`? What goes in `source_ref`? What `Context` (a bounce has no IP and
    no user agent belonging to the subscriber, and `token_jti` must stay `None`)?
22. **Soft vs. hard bounces.** The `Reason` enum has one `Bounced`. Nothing distinguishes a transient
    4xx from a permanent 5xx, and the decomposition wants "4xx retry with backoff, 5xx bounce,
    ambiguous outcomes marked `unknown` and never auto-retried". `unknown` has no home in any
    existing enum.
23. **Spec §13.3 / §13.4, named as the project's two unanswered items** in the decomposition:
    whether OCI Email Delivery's DKIM signature covers the `List-Unsubscribe` headers (if not,
    in-app DKIM stops being optional and RFC 8058 one-click is not honoured by Gmail and Yahoo), and
    how OCI reports bounces and complaints. §13.4 requires the DKIM question to be verified before
    M2 is signed off. Neither is answerable from this repository.

**Schema and migrations**

24. **New lock-order rulings.** `campaigns`, `templates`, `relays`, `sending_domains` and `messages`
    are new foreign-key parents. The ADR's enumeration is closed and mechanical; each new parent is
    a row somebody has to write and justify, and `messages` referencing both `contacts` and
    `lists` makes it a class-4-shaped table on day one.
25. **Erasure versus send history.** `contacts` cascades into `list_memberships`, `consent_log`,
    `contact_tags` and `jobs`. Whether `messages` cascades too — destroying the record that a
    campaign was delivered — or keeps a tombstone, is a GDPR decision with precedent on both sides
    (`consent_log` cascades; `suppressions` survives as an HMAC-only tombstone).
26. **Indexing a large `messages` table later.** Every migration runs in a transaction because of
    the SPDX header convention, so `create index concurrently` is impossible without a carve-out in
    `scripts/check-spdx.sh` that nobody has made (`0026:233-240`, `issues/24`). Every index
    `messages` will ever need either goes in at creation or costs an `ACCESS EXCLUSIVE` lock for the
    length of a build.
27. **Timezone sends.** `contacts.timezone` is a validated IANA string and `chrono-tz` is available,
    but nothing decides how "send at 09:00 recipient-local" is scheduled: 24+ `run_after` buckets per
    campaign, one job per zone, or a per-recipient `run_after` on `messages`. Nor what happens to a
    contact with a null timezone.

**Dependencies and build**

28. **SMTP client choice**, and whether it is vendored or a crate. `deny.toml` and
    `web/scripts/check-licenses.mjs` gate it; the `page.rs` precedent ("a dependency for five
    substitutions is a dependency with an audit attached") sets the bar.
29. **mrml / MJML.** Not in-tree. Its licence, its binary size and whether it runs in the server or
    only in the editor build are undecided.
30. **`AppState` growth.** A relay pool, a DKIM signer and a per-relay token bucket all want to live
    somewhere. Putting them in `AppState` means every integration test constructs them
    (`tests/common/mod.rs:49`); putting them in a worker-only struct means the HTTP side cannot do a
    test send. Nobody has chosen.
31. **Worker metrics naming.** Only one bespoke counter exists. There is no established convention
    for worker-side metrics (per-kind latency, claim contention, relay failures), and
    `metrics_layer`'s cardinality discipline ("never the raw path") is the only guidance.
