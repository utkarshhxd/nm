# M2a — relays, sending domains, and the first real send: implementation plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development
> (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use
> checkbox (`- [ ]`) syntax for tracking.

**Goal:** A double opt-in confirmation email enqueued by an M1d form is built, signed, and
delivered through a configured relay, and the subscriber's link confirms their subscription.

**Architecture:** One migration adds `relays`, `sending_domains` and `relay_buckets`. A new
`nm worker` mode of the existing binary — with its own connection pool — drains `jobs` using the
claim protocol `comment on table jobs` already specifies, re-checks suppression at send time,
builds a MIME message carrying our own RFC 8058 unsubscribe headers, optionally DKIM-signs it
in-process, debits a Postgres-held token bucket for the relay, and submits over SMTP. Outcomes are
classified into four buckets, one of which is never retried.

**Tech Stack:** Rust 2024, axum, sqlx/PostgreSQL 16, lettre (SMTP + MIME), ring (RSA-SHA256 for
DKIM), hickory-resolver (DNS checker), React/Vite for the settings screens.

**Spec:** `.scratch/email-platform/specs/2026-11-21-m2a-relays-and-sending-domains.md`
Supporting: `.scratch/email-platform/research/m2-oci-email-delivery.md`,
`.scratch/email-platform/m2-decomposition.md`, `docs/adr/0005-one-lock-order-for-every-path.md`.

---

## Global Constraints

- **Every cargo, sqlx and pnpm command runs in the Linux dev container**: `bash scripts/dev.sh
  cargo test`. Windows cannot link Rust here. Compose project is always `-p nm` (`scripts/dev.sh`
  already passes it). Never stop, modify or remove the containers `oci-email-postgres` or
  `marketing-automation` — they belong to other projects.
- **Every tenant table has a `workspace_id` column.** Asking for another workspace's resource
  returns `404`. It never returns `403`, and it never returns the data.
- **Secrets never appear in logs, audit metadata, or API responses** — spec §8 invariant 7. A relay
  password and a DKIM private key are secrets. The API returns *whether* one is set.
- **Every migration file starts with the SPDX header**, so `sqlx migrate!` wraps it in a
  transaction and `create index concurrently` is impossible. Every index a table will need goes in
  at creation.
- **Nothing deletes a `jobs` row.** `jobs_throttle_retention` refuses a delete inside the retention
  window, because counting those rows is the signup form's only defence against being an open
  mailer.
- **`docs/adr/0005-one-lock-order-for-every-path.md` binds every new writer**, and a new foreign-key
  parent or cascade means re-running its enumeration, not merely obeying the order.
- **Route modules merge in `http::routes()`**, never in `app::router` — a `.merge` added after the
  CSRF layer silently gets neither CSRF nor cookie stripping.
- **After any route or schema change**, run `bash scripts/dev.sh pnpm --dir web gen:api` (both
  halves; CI fails if either has drifted).
- **Gate, all green, before each task's commit:** `cargo test`, `cargo clippy --all-targets -- -D
  warnings`, `cargo fmt --check`, `cargo deny check`, `bash scripts/check-spdx.sh`. Web tasks also:
  `pnpm --dir web build`, `check:bundle`, `check:licenses`.
- **The e2e harness may only touch databases matching `nm_e2e_[0-9a-f]{12}`.** Never `nm`, never
  `postgres`.
- **`254 4.7.1` is a delivery failure, not a success** (spec §5.8).
- Never rewrite history on shared branches; never commit directly to `main`.

## Dependency rulings

These settle what the spec left open. Each is a decision with its cost.

**D1 — `lettre` provides both the SMTP transport and the MIME builder.** One crate for two jobs
beats two crates. Features: async tokio executor, rustls with the **ring** provider (matching
sqlx's `tls-rustls-ring-native-roots`, so there is one rustls backend in the tree), `pool`, and
`builder`. **Not its `dkim` feature** — see D2. Messages are always submitted with lettre's
raw-bytes send, so signed and unsigned messages travel one code path.
*Cost if wrong:* lettre's `Message` builder is less flexible than hand-rolled MIME; if it cannot
express a header we need, that header is added with lettre's custom-header mechanism rather than by
replacing the crate.

**D2 — DKIM signing is ours, over `ring`, and no DKIM crate is added.** Every Rust DKIM signer
(including lettre's own `dkim` feature and `mail-auth`) performs RSA private-key operations through
the `rsa` crate. `deny.toml`'s RUSTSEC-2023-0071 ignore rests entirely on the claim that *"NM holds
no RSA private key and performs no RSA private-key operation"*, and says in terms: **drop the
ignore rather than extend it** if a new caller reaches it. `ring` is already in the dependency tree
via sqlx's rustls-ring and signs RSA-PKCS#1-SHA256 in constant time, so the ignore stays true and
unamended. Canonicalization is text normalization, not cryptography, and RFC 6376 §3.4.5 publishes
test vectors for it.
*Cost if wrong:* roughly 200 lines of relaxed/relaxed canonicalization we own and must keep correct,
against a crate we would not have had to write. If a future milestone needs DKIM *verification*
too, revisit — verification is a public-key operation and the ignore does not block it.

**D3 — `hickory-resolver` for the DNS checker.** TXT and CNAME lookups with an explicit timeout; no
realistic alternative in Rust, and shelling out to `dig` is not a dependency, it is a deployment
requirement.

**D4 — the operator supplies the DKIM private key; the product does not generate one.** `ring`
cannot generate RSA keys, and adding a crate that can reopens D2. The settings screen prints the
exact `openssl` command and, once a key is saved, the `v=DKIM1; k=rsa; p=…` value to publish —
derived from the key the operator pasted, so the two cannot disagree.
*Cost if wrong:* one more manual step during domain setup. The DNS checker compares the published
`p=` against the stored key, so a mismatch is caught rather than discovered as a signature failure.

**D5 — anything that could have been accepted is `unknown`, and `unknown` is never retried.**
Outcomes are classified from lettre's SMTP error kinds: a failure to connect, negotiate TLS, or
read the greeting is `Transient` (nothing was submitted, retrying is safe); a 4xx reply including
`455` and `421` is `Transient`; a 5xx reply is `Failed`; **any network or I/O error once the
session is established is `Unknown`**, because we cannot prove the message did not arrive.
*Cost if wrong:* a network error during `EHLO` — before anything was submitted — is also treated as
`Unknown`, so a message that could have been safely retried is lost instead. That is the
conservative direction, and spec §5.8 chooses it: a second copy of a confirmation email is
indistinguishable from the product being broken.

## Review Focus

Five things the spec implies that no task's own happy path exercises. Each has its test added to
the task that owns the code.

1. **A relay that accepts the connection and never replies.** With no explicit timeout the worker's
   tick blocks past the 120-second lease, the row is re-claimed by another worker, and the message
   goes twice — the one outcome §4 names as worse than not sending. *Owner: Task 7.*
2. **A `jobs.payload` without `email` or `token`, or with a non-string in one.** A hand-edited row
   or one from an older build must fail that row with a reason, not panic the worker or poison the
   whole tick. *Owner: Task 10.*
3. **A DNS lookup that times out or NXDOMAINs while an operator watches the settings page.** The
   check must answer within a bounded time with a recorded failure, never hold the request open or
   the pooled connection with it. *Owner: Task 4.*
4. **A pasted DKIM key that is not a key, is a public key, or is under 2048 bits.** Rejected at save
   time with a message the operator can act on, never a panic at send time inside a worker tick.
   *Owner: Task 6.*
5. **A non-ASCII address or display name** (IDN domain, UTF-8 local part). The builder must either
   encode it correctly or refuse with a reason *before* a claim has been spent on it. *Owner: Task 5.*

---

## File Structure

| File | Responsibility |
|---|---|
| `migrations/0028_relays_and_sending_domains.sql` | `relays`, `sending_domains`, `relay_buckets` |
| `src/relay/mod.rs` | `Relay`, `Security`, lookup, credential decryption |
| `src/relay/routes.rs` | relay CRUD, audit, never the password |
| `src/relay/transport.rs` | lettre transports per relay, send, outcome classification |
| `src/relay/bucket.rs` | the Postgres token bucket |
| `src/sending_domains/mod.rs` | `SendingDomain`, `Signing`, lookup, key decryption |
| `src/sending_domains/routes.rs` | CRUD, the check endpoint, audit |
| `src/sending_domains/dns.rs` | SPF / DKIM / DMARC / Return-Path checks |
| `src/mail/mod.rs` | MIME construction and the required headers |
| `src/mail/dkim.rs` | relaxed/relaxed canonicalization, RSA-SHA256 signing, SPKI helpers |
| `src/worker/mod.rs` | claim loop, lease, backoff, `run` |
| `src/worker/confirmation.rs` | the `confirmation_email` handler |
| `tests/common/smtp_sink.rs` | an in-process SMTP server tests drive to any reply |
| `web/src/routes/Relays.tsx`, `web/src/routes/SendingDomains.tsx` | settings screens |

---

### Task 1: Migration 0028 — relays, sending domains, token buckets

**Files:**
- Create: `migrations/0028_relays_and_sending_domains.sql`
- Create: `tests/relays.rs`

**Interfaces:**
- Consumes: `workspaces(id)`, the `0026`/`0027` conventions for composite foreign keys.
- Produces: the three tables every later task reads. Column names are fixed here.

`relays`: `id uuid pk`, `workspace_id uuid not null references workspaces(id) on delete cascade`,
`name text not null`, `host text not null`, `port int not null check (port between 1 and 65535)`,
`security text not null check (security in ('implicit_tls','starttls'))`, `username text`,
`password_encrypted bytea`, `max_connections int not null default 4 check (max_connections > 0)`,
`rate_limit_per_minute int not null check (rate_limit_per_minute > 0)`, `enabled bool not null
default true`, `failover_order int not null default 0`, `created_at`, `updated_at`.
Unique on `(workspace_id, name)`. Index on `(workspace_id, failover_order, id) where enabled`.
Unique on `(workspace_id, id)` — the composite parent key the bucket's foreign key needs.

`sending_domains`: `id uuid pk`, `workspace_id uuid not null references workspaces(id) on delete
cascade`, `domain text not null`, `from_name text`, `signing text not null check (signing in
('relay','app'))`, `dkim_selector text not null`, `dkim_private_key_encrypted bytea`,
`return_path_domain text`, `checks jsonb not null default '{}'::jsonb`, `checked_at timestamptz`,
`created_at`, `updated_at`. Unique on `(workspace_id, domain)`. Check: `signing = 'relay' or
dkim_private_key_encrypted is not null` — an app-signed domain without a key cannot exist.

`relay_buckets`: `relay_id uuid primary key`, `workspace_id uuid not null`, foreign key
`(workspace_id, relay_id) references relays (workspace_id, id) on delete cascade`, `tokens int not
null`, `refilled_at timestamptz not null default now()`.

A `comment on table` on each, stating what it is for. On `relay_buckets`, state that the bucket is
the only thing standing between us and a `455` the relay publishes no `Retry-After` for.

- [ ] **Step 1: Write the failing test** — `tests/relays.rs`, using the `sqlx::test` private-database
  pattern the other test files use:
  - `relay_requires_a_workspace`: inserting a relay with a random `workspace_id` raises `23503`.
  - `deleting_a_workspace_takes_its_relays_and_buckets`: insert workspace, relay, bucket; delete the
    workspace; both child rows are gone.
  - `an_app_signed_domain_without_a_key_is_refused`: insert `signing = 'app'` with a null key, expect
    `23514`.
  - `two_relays_cannot_share_a_name_in_one_workspace`: expect `23505`; the same name in a second
    workspace succeeds.
  - `a_bucket_cannot_point_at_another_workspaces_relay`: insert a bucket whose `workspace_id` is a
    second workspace's and whose `relay_id` is the first's; expect `23503`.
- [ ] **Step 2: Run it and watch it fail** — `bash scripts/dev.sh cargo test --test relays`.
  Expected: every test fails, the tables do not exist.
- [ ] **Step 3: Write the migration.** SPDX header first line. Tables, checks, indexes and comments
  as above.
- [ ] **Step 4: Run the tests** — same command. Expected: PASS.
- [ ] **Step 5: Run the full gate** and commit.

---

### Task 2: Re-run ADR 0005's enumeration for the new parents

Spec §5.10 makes this its own deliverable. `issues/38` records why: a new foreign-key parent means
re-running the enumeration, not merely obeying the order. That method is what caught `form_lists`
after the order was thought complete.

**Files:**
- Modify: `docs/adr/0005-one-lock-order-for-every-path.md`
- Modify: `tests/lock_order.rs`

**Interfaces:**
- Consumes: Task 1's tables.
- Produces: the lock class (or the ruling that there is none) for `relays` and `sending_domains`,
  which every later writer obeys.

- [ ] **Step 1: Re-derive the enumeration from the database, not from the document.** In the dev
  container against a migrated database, list every foreign-key parent and its children from
  `pg_constraint` (`contype = 'f'`), and diff that against the nine parents the ADR's table names.
  Record the command used in the ADR, so the next milestone re-runs the same query.
- [ ] **Step 2: Rule on each new parent**, in the ADR's existing table shape (`parent | ruling |
  why`). For each of `relays` and `sending_domains`, answer the ADR's own question: does any path
  take an **exclusive** lock on the parent row while also writing a child, or write a child while
  another path holds the parent? Remember the `FOR KEY SHARE` / `FOR NO KEY UPDATE` distinction the
  ADR records — an update touching only non-key columns does not conflict with a foreign key's
  `FOR KEY SHARE`, which is why `workspaces`, `users` and `api_keys` have no exclusive taker.
- [ ] **Step 3: Extend the path table** with every writer this plan adds (Tasks 3, 4, 8, 9, 10),
  in the same `path | order it takes` shape.
- [ ] **Step 4: Write the concurrency tests** for any pair the enumeration rules a class, in
  `tests/lock_order.rs`, following that file's existing pattern: two transactions gated to two at a
  time, each waiting on `pg_stat_activity` for the other to be where it is supposed to be, and
  **panicking if the interleaving never forms** rather than passing vacuously. If the enumeration
  rules that no new class exists, write instead one test that *demonstrates* the reason — for
  example that deleting a relay while its bucket is being debited raises `23503` or `40P01` and
  which — and say in the ADR that the test is the evidence for the ruling.
- [ ] **Step 5: Run `cargo test --test lock_order`** and the full gate. Expected: PASS, with the new
  tests visible in the count.
- [ ] **Step 6: Commit.**

---

### Task 3: Relay configuration — CRUD, encrypted credentials, audit

**Files:**
- Create: `src/relay/mod.rs`, `src/relay/routes.rs`
- Modify: `src/lib.rs` (`pub mod relay;`), `src/http/mod.rs` (merge), `src/openapi.rs`,
  `src/audit/mod.rs` (new `actions` constants, named like the existing ones)
- Test: `tests/relays.rs`

**Interfaces:**
- Consumes: `AppState`, `crypto::secrets::Secrets`, `audit::record`, `error::ApiResult`.
- Produces:
  ```rust
  pub enum Security { ImplicitTls, StartTls }
  pub struct Relay {
      pub id: Uuid, pub workspace_id: Uuid, pub name: String, pub host: String,
      pub port: i32, pub security: Security, pub username: Option<String>,
      pub max_connections: i32, pub rate_limit_per_minute: i32,
      pub enabled: bool, pub failover_order: i32,
  }
  pub async fn enabled_relays(db: impl PgExecutor<'_>, workspace_id: Uuid)
      -> ApiResult<Vec<Relay>>;                       // ordered by failover_order, then id
  pub async fn password(db: impl PgExecutor<'_>, secrets: &Secrets,
      workspace_id: Uuid, relay_id: Uuid) -> ApiResult<Option<String>>;
  ```

Routes: `GET/POST /api/v1/workspaces/{workspace_id}/relays`,
`GET/PATCH/DELETE /api/v1/workspaces/{workspace_id}/relays/{id}`, following the shape
`src/suppressions/routes.rs` uses for scoping, roles and pagination. The response carries
`has_password: bool` and never the password. `PATCH` with no `password` key leaves the stored one
alone; `password: null` clears it — use `Option<Option<String>>` with
`#[serde(default, deserialize_with = …)]` the way the codebase already distinguishes absent from
null, and `deny_unknown_fields`.

- [ ] **Step 1: Write the failing tests** in `tests/relays.rs`:
  - `a_created_relay_is_listed_without_its_password`: create with a password; neither the create
    response nor the list response contains the password string anywhere in its serialized body;
    `has_password` is `true`.
  - `the_password_round_trips_through_secrets`: `relay::password(...)` returns exactly what was
    posted.
  - `the_audit_row_for_a_relay_carries_no_password`: after create and update, no `audit_log` row's
    `metadata::text` contains the password.
  - `another_workspaces_relay_is_404`: a member of workspace B asking for workspace A's relay gets
    `404`, not `403`, and no body field names the relay.
  - `patch_without_a_password_key_keeps_the_stored_one`, and `password: null` clears it.
  - `enabled_relays_orders_by_failover_then_id`, and excludes disabled ones.
- [ ] **Step 2: Run and watch them fail.**
- [ ] **Step 3: Implement `src/relay/mod.rs` and `routes.rs`.** Store the password with
  `secrets.encrypt`. Audit on create, update and delete with the existing `AuditEvent` shape;
  metadata names the relay and host, never the credential.
- [ ] **Step 4: Register** the module in `src/lib.rs`, the router in `http::routes()`, the schemas in
  `src/openapi.rs`, then run `pnpm --dir web gen:api`.
- [ ] **Step 5: Run the tests and the full gate.** Commit.

---

### Task 4: Sending domains — CRUD and the DNS checker

**Files:**
- Create: `src/sending_domains/mod.rs`, `src/sending_domains/routes.rs`, `src/sending_domains/dns.rs`
- Modify: `src/lib.rs`, `src/http/mod.rs`, `src/openapi.rs`, `src/audit/mod.rs`, `Cargo.toml`
- Test: `tests/sending_domains.rs`

**Interfaces:**
- Consumes: Task 3's module layout as the pattern; `Secrets`.
- Produces:
  ```rust
  pub enum Signing { Relay, App }
  pub enum CheckResult { Pass, Fail(String), NotChecked }
  pub struct DnsChecks { pub spf: CheckResult, pub dkim: CheckResult,
                         pub dmarc: CheckResult, pub return_path: CheckResult }
  pub struct SendingDomain {
      pub id: Uuid, pub workspace_id: Uuid, pub domain: String,
      pub from_name: Option<String>, pub signing: Signing,
      pub dkim_selector: String, pub return_path_domain: Option<String>,
      pub checks: DnsChecks, pub checked_at: Option<DateTime<Utc>>,
  }
  pub async fn for_domain(db: impl PgExecutor<'_>, workspace_id: Uuid, domain: &str)
      -> ApiResult<Option<SendingDomain>>;
  pub async fn dkim_private_key(db: impl PgExecutor<'_>, secrets: &Secrets,
      workspace_id: Uuid, id: Uuid) -> ApiResult<Option<Vec<u8>>>;   // DER, decrypted
  pub mod dns {
      pub async fn check(resolver: &Resolver, d: &SendingDomain, expected_p: Option<&str>)
          -> DnsChecks;
  }
  ```
- **`expected_p` has no caller yet in this task.** Deriving it from the stored key needs
  `mail::dkim::public_key_txt`, which Task 6 builds. This task implements and tests the comparison
  against literal values and passes `None` from the route; **Task 6 Step 5 is where the route starts
  supplying it.** Do not stub `public_key_txt` here — two implementations of the same derivation is
  how a mismatch becomes invisible.

What each check asserts: **SPF** — a `v=spf1` TXT record exists at the domain and is the only one.
**DKIM** — a TXT record at `{selector}._domainkey.{domain}` parses as `v=DKIM1` with a `p=`, and
when `expected_p` is given, matches it. **DMARC** — a `v=DMARC1` TXT record at `_dmarc.{domain}`.
**Return-Path** — when `return_path_domain` is set, it has a CNAME, and its organizational domain
aligns with `domain` (relaxed alignment: equal, or one is a subdomain of the other).

- [ ] **Step 1: Write the failing tests** in `tests/sending_domains.rs` and unit tests in `dns.rs`:
  - Unit, over the *parsing* rather than the network: `v=spf1 …` passes, two SPF records fail with a
    reason naming the duplicate, a `v=DKIM1` record with no `p=` fails, a `p=` that differs from the
    expected one fails with a reason that does **not** print either key, relaxed alignment accepts
    `bounce.mail.example.com` against `example.com` and rejects `example.net`.
  - **Review Focus 3:** `a_lookup_that_times_out_is_a_recorded_failure` — a resolver pointed at a
    black-holed address returns `Fail` within the configured timeout, and `check` as a whole returns
    in under twice that. Assert the elapsed time.
  - Integration: `a_sending_domain_hides_its_private_key` — no response body and no audit row
    contains the key material; `has_dkim_key` is `true`.
  - `another_workspaces_domain_is_404`.
  - `app_signing_without_a_key_is_refused_by_the_api_with_a_field_error`, not a `500` from the
    database check constraint.
- [ ] **Step 2: Run and watch them fail.**
- [ ] **Step 3: Add `hickory-resolver` to `Cargo.toml`** (D3) and run `cargo deny check` before
  writing code — a licence refusal is cheaper to find now.
- [ ] **Step 4: Implement** the module, routes and checker. The resolver is built once and held in
  `AppState`; every lookup carries an explicit timeout, and `check` runs the four lookups
  concurrently with `tokio::join!` so a slow one does not serialise the rest.
- [ ] **Step 5:** `POST /api/v1/workspaces/{id}/sending-domains/{id}/check` runs the checker, stores
  `checks` and `checked_at`, and returns them. It is rate-limited per workspace through the existing
  `RateLimiter`, because it makes outbound network calls on an authenticated request.
- [ ] **Step 6: Run the tests, `gen:api`, and the full gate.** Commit.

---

### Task 5: Message construction and the headers a bulk sender must carry

**Files:**
- Create: `src/mail/mod.rs`
- Modify: `src/lib.rs`, `Cargo.toml`
- Test: unit tests in `src/mail/mod.rs`

**Interfaces:**
- Consumes: `config::Config::public_url` for the unsubscribe URL's origin.
- Produces:
  ```rust
  pub struct Outgoing<'a> {
      pub from: &'a str, pub from_name: Option<&'a str>, pub to: &'a str,
      pub subject: &'a str, pub text: &'a str, pub html: Option<&'a str>,
      pub unsubscribe_url: &'a url::Url, pub feedback_id: &'a str,
      pub message_id_domain: &'a str,
  }
  pub fn build(o: &Outgoing) -> Result<lettre::Message, MailError>;
  /// The `h=` list Task 6 signs. Contains "List-Unsubscribe" and "List-Unsubscribe-Post".
  pub const SIGNED_HEADERS: &[&str];
  ```

Every built message carries, per spec §5.5: `List-Unsubscribe: <https://…/u/{token}>` — **ours**,
never a relay's — `List-Unsubscribe-Post: List-Unsubscribe=One-Click`, `Feedback-ID`, and a
`Message-ID` whose right-hand side is `message_id_domain`. A `text/plain` part is always present;
`html`, when given, makes the message `multipart/alternative` with plain first.

- [ ] **Step 1: Write the failing tests:**
  - `both_unsubscribe_headers_are_present_and_ours`: the built message's wire bytes contain
    `List-Unsubscribe: <https://example.test/u/TOK>` and
    `List-Unsubscribe-Post: List-Unsubscribe=One-Click`, and contain no `mailto:` unsubscribe.
  - `signed_headers_covers_both_unsubscribe_headers`: `SIGNED_HEADERS` contains both, compared
    case-insensitively.
  - `message_id_is_on_the_sending_domain`: the `Message-ID` right-hand side equals
    `message_id_domain`, and two builds differ in the left-hand side.
  - `a_plain_text_part_always_exists`, and with `html` set the message is `multipart/alternative`
    with `text/plain` first.
  - **Review Focus 5:** `a_non_ascii_display_name_is_encoded` — a `from_name` of `Grüße` appears
    RFC 2047-encoded, not raw; `an_idn_recipient_domain_is_rejected_with_a_reason` — building for
    `user@exämple.test` returns `Err(MailError::…)` naming the address class, and **does not panic**.
    (Refusing is the ruling: punycode conversion would need another dependency and OCI's approved-
    sender rules are ASCII anyway. The error is explicit so the worker can fail that one row.)
- [ ] **Step 2: Run and watch them fail.**
- [ ] **Step 3: Add `lettre` to `Cargo.toml`** per D1, with the ring-backed rustls feature. Run
  `cargo deny check` before writing code.
- [ ] **Step 4: Implement `build`.** `List-Unsubscribe`, `List-Unsubscribe-Post` and `Feedback-ID`
  are custom headers — add them through lettre's custom-header mechanism; keep each header's name
  spelled once, in a constant that `SIGNED_HEADERS` also uses, so the two cannot drift.
- [ ] **Step 5: Run the tests and the full gate.** Commit.

---

### Task 6: In-app DKIM signing

**Files:**
- Create: `src/mail/dkim.rs`
- Modify: `src/mail/mod.rs` (`pub mod dkim;`), `Cargo.toml` (`ring`, as a direct dependency)
- Test: unit tests in `src/mail/dkim.rs`

**Interfaces:**
- Consumes: Task 5's `SIGNED_HEADERS`.
- Produces:
  ```rust
  pub struct SigningKey(ring::signature::RsaKeyPair);
  pub fn key_from_pem(pem: &str) -> Result<SigningKey, DkimError>;
  /// `v=DKIM1; k=rsa; p=<base64 SubjectPublicKeyInfo>` — the TXT value to publish.
  pub fn public_key_txt(key: &SigningKey) -> String;
  /// The full wire message with a `DKIM-Signature` header prepended.
  pub fn signed_bytes(msg: &lettre::Message, key: &SigningKey,
                      domain: &str, selector: &str) -> Result<Vec<u8>, DkimError>;
  ```

Algorithm: `rsa-sha256`, relaxed header canonicalization, relaxed body canonicalization, `h=`
exactly `SIGNED_HEADERS` restricted to the headers actually present and in the order RFC 6376
requires, `bh=` over the canonicalized body, `b=` computed with the `DKIM-Signature` header itself
canonicalized with an empty `b=` and appended last.

`public_key_txt` must emit **SubjectPublicKeyInfo**, which is what a DKIM `p=` carries, while
`ring` exposes the PKCS#1 `RSAPublicKey`. The conversion is a fixed DER wrapper (`SEQUENCE {
SEQUENCE { OID 1.2.840.113549.1.1.1, NULL }, BIT STRING { … } }`); write it and its inverse
(`spki_to_pkcs1`) in this file, because Task 4's checker needs the inverse to compare a published
`p=` against the stored key.

- [ ] **Step 1: Write the failing tests:**
  - `relaxed_header_canonicalization_matches_rfc_6376`: the §3.4.5 example input canonicalizes to the
    published output, byte for byte.
  - `relaxed_body_canonicalization_matches_rfc_6376`: same, including that a body of only CRLFs
    canonicalizes to a single CRLF and an empty body to the empty string.
  - `h_covers_both_unsubscribe_headers`: sign a message built by Task 5; the emitted `h=` tag,
    lowercased and split on `:`, contains `list-unsubscribe` and `list-unsubscribe-post`.
  - `a_signature_verifies_against_the_published_public_key`: sign, then verify `b=` with
    `ring`'s RSA verification using the key `public_key_txt` emits, round-tripped through
    `spki_to_pkcs1`. Then flip one byte of the body and assert verification fails — without that
    second half the first proves only that signing produced *something*.
  - `spki_round_trips`: `spki_to_pkcs1(pkcs1_to_spki(k)) == k`.
  - **Review Focus 4:** `key_from_pem_refuses_what_is_not_a_private_key` — a public-key PEM, a
    1024-bit key, an EC key, truncated base64 and empty input each return a distinct `DkimError`
    variant whose message names the problem, and **none panics**.
- [ ] **Step 2: Run and watch them fail.** Generate the 2048-bit and 1024-bit test keys once with
  `openssl genrsa` and commit them under `tests/fixtures/dkim/` with a README line saying they are
  throwaway test keys and must never be used by an instance.
- [ ] **Step 3: Add `ring = "0.17"` to `Cargo.toml`** as a direct dependency. `Cargo.lock` already
  resolves `ring 0.17.14` through `rustls 0.23.46`, so no new version enters the tree — verify that
  with `cargo tree -d -i ring` after adding it, then run `cargo deny check`.
- [ ] **Step 4: Implement** canonicalization, the SPKI helpers, `key_from_pem` (accepting both
  PKCS#1 `BEGIN RSA PRIVATE KEY` and PKCS#8 `BEGIN PRIVATE KEY` PEM), and `signed_bytes`.
- [ ] **Step 5: Wire both directions into Task 4.** `key_from_pem` is called before the key is
  encrypted, so an unusable key is a field error at save time, never a panic in a worker. And the
  check route now derives `expected_p` from the stored key with `public_key_txt` instead of passing
  `None`, so a published record that does not match the key we sign with becomes a failing check.
  Add the integration test that proves it: save a key, publish a *different* `p=` in the test
  resolver, assert the DKIM check fails and that the failure text names neither key.
- [ ] **Step 6: Run the tests and the full gate.** Commit.

---

### Task 7: SMTP transport and outcome classification

**Files:**
- Create: `src/relay/transport.rs`, `tests/common/smtp_sink.rs`
- Modify: `src/relay/mod.rs`, `tests/common/mod.rs`
- Test: `tests/relay_transport.rs`

**Interfaces:**
- Consumes: Task 3's `Relay`, Task 5's message bytes.
- Produces:
  ```rust
  pub enum Outcome {
      Sent { response: String },
      Transient(String),
      Failed(String),
      Unknown(String),
  }
  /// Holds one pooled lettre transport per relay id, rebuilt when the relay's config changes.
  pub struct Transports { /* … */ }
  impl Transports {
      pub fn new() -> Transports;
      pub async fn send(&self, relay: &Relay, password: Option<&str>,
                        envelope: &lettre::address::Envelope, bytes: &[u8]) -> Outcome;
  }
  pub const CONNECT_TIMEOUT: Duration;   // 10 s
  pub const SESSION_TIMEOUT: Duration;   // 60 s — under the 120 s lease, with margin
  ```

Classification per D5. One extra rule from the research: a `2xx` whose text begins `254 4.7.1`
cannot occur (254 is not 2xx-accepted in lettre's reading), but OCI returns **`250` with a
suppression notice**, and separately may answer `254`. Treat *any* accepted reply whose text
contains `Suppression for user` as `Failed`, with the reply text as the reason — spec §5.8.

`smtp_sink`: a tokio `TcpListener` that speaks enough SMTP (greeting, `EHLO`, `MAIL`, `RCPT`,
`DATA`, `.`) and is constructed with a script of what to reply and when to hang up, so a test can
produce any of the five outcomes deterministically. Plain TCP only — the transport is built against
`127.0.0.1` with TLS disabled for these tests.

- [ ] **Step 1: Write the failing tests** in `tests/relay_transport.rs`, one per outcome, each
  driving `smtp_sink`:
  - `a_250_is_sent`, and the recorded response text is the sink's.
  - `a_451_is_transient`; `a_550_is_failed`.
  - `a_455_is_transient` — the OCI throttle reply.
  - `a_250_naming_a_suppression_is_failed`, not sent, and the reason carries the reply.
  - `a_drop_after_data_is_unknown`: the sink reads the body then closes without replying.
  - `a_refused_connection_is_transient`: nothing was submitted.
  - **Review Focus 1:** `a_relay_that_never_replies_does_not_outlive_the_lease` — the sink accepts
    and then sleeps indefinitely; `send` returns within `SESSION_TIMEOUT`, the test asserts the
    elapsed time is under the 120-second lease, and the outcome is `Unknown` (a hang after `DATA`
    could have been accepted).
- [ ] **Step 2: Run and watch them fail.**
- [ ] **Step 3: Implement `smtp_sink`**, then `Transports::send`. Set lettre's connect and overall
  timeouts explicitly from the two constants; do not rely on a default.
- [ ] **Step 4: Run the tests and the full gate.** Commit.

---

### Task 8: The relay token bucket

**Files:**
- Create: `src/relay/bucket.rs`
- Modify: `src/relay/mod.rs`
- Test: `tests/relay_bucket.rs`

**Interfaces:**
- Consumes: Task 1's `relay_buckets`.
- Produces:
  ```rust
  /// Refills by elapsed time (capped at the relay's per-minute limit), then takes `n`.
  /// `false` means not enough tokens; the caller does not send this tick.
  pub async fn take(db: impl PgExecutor<'_>, workspace_id: Uuid, relay_id: Uuid,
                    limit_per_minute: i32, n: i32) -> sqlx::Result<bool>;
  ```

One statement: `insert … on conflict (relay_id) do update` that refills by
`floor(extract(epoch from now() - refilled_at) * limit / 60)` capped at `limit`, subtracts `n`, and
is predicated on the refilled value being `>= n`, `returning` whether a row came back. Debited
**once per claimed batch**, never per message (spec §5.4).

**This statement is load-bearing outside this task, and the plan owes you the reason.** ADR 0005's
ruling that `relays` is *not* a lock class was derived against this exact shape, and
`tests/lock_order.rs` carries a reconstruction of it. The ruling turns on two properties:

- the **insert arm** takes `for key share` on the `relays` row and holds nothing else waitable, so
  it asks for one lock and cannot be half of a cycle;
- the **update arm** writes only `tokens` and `refilled_at`, which are not key columns, so it fires
  no referential trigger and takes nothing on the parent at all.

**If your `take` departs from that — if it reads or locks a `relays` row, if it writes a
foreign-key column on conflict, or if it becomes more than one statement — the ruling no longer
describes your code and ADR 0005's enumeration must be re-run before you commit.** Say in your
report which of the two arms you wrote and whether either property changed. `limit_per_minute` is
a parameter precisely so `take` never has to read `relays`; keep it that way.

**Carried from Task 1's review:** a *first* take with `n > limit_per_minute` inserts
`limit_per_minute - n`, which is negative, and `0028`'s `relay_buckets_tokens_non_negative` now
makes that a `23514` rather than the `false` the signature promises. Clamp or predicate the insert
arm, and add `a_take_larger_than_the_limit_returns_false_rather_than_raising` to the tests below.

- [ ] **Step 1: Write the failing tests:**
  - `a_fresh_bucket_allows_up_to_the_limit_and_then_refuses`.
  - `tokens_refill_with_time`: set `refilled_at` back 30 seconds on a limit of 60, assert exactly 30
    more are available and not 31.
  - `refill_is_capped_at_the_limit`: `refilled_at` an hour ago does not allow more than `limit`.
  - `concurrent_takers_never_exceed_the_limit` — **invariant 8**: spawn 20 tasks each taking 5
    against a limit of 50, on their own connections; assert the successes total **exactly** 50, not
    "at most". A test that asserts only an upper bound passes on a bucket that refuses everyone.
  - `taking_zero_is_refused` — a batch that thinks it has no messages must not spend a round trip or
    silently succeed.
- [ ] **Step 2: Run and watch them fail.**
- [ ] **Step 3: Implement `take`.**
- [ ] **Step 4: Run the tests and the full gate.** Commit.

---

### Task 9: The worker — claim, lease, completion and backoff

*(The tick that calls these is Task 11's `worker::run`; this task builds the primitives it uses.)*

**Files:**
- Create: `src/worker/mod.rs`
- Modify: `src/lib.rs`
- Test: `tests/worker_claim.rs`

**Interfaces:**
- Consumes: `jobs`, whose claim protocol is stated by `comment on table jobs` in
  `migrations/0026_jobs_references_and_consent_guards.sql`. **Read that comment; it supersedes the
  query in `0023`, and re-deriving it is how the last milestone's defects happened.**
- Produces:
  ```rust
  pub const LEASE_SECS: i64 = 120;
  pub const BATCH: i64 = 16;
  pub const POLL: Duration = Duration::from_secs(1);   // with jitter
  pub struct Claimed {
      pub id: Uuid, pub workspace_id: Uuid, pub payload: serde_json::Value,
      pub contact_id: Option<Uuid>, pub list_id: Option<Uuid>,
      pub attempts: i32, pub max_attempts: i32, pub created_at: DateTime<Utc>,
  }
  pub async fn claim(db: impl PgExecutor<'_>, kind: &str, batch: i64, lease_secs: i64)
      -> sqlx::Result<Vec<Claimed>>;
  pub async fn complete(db: impl PgExecutor<'_>, id: Uuid) -> sqlx::Result<()>;
  pub async fn fail(db: impl PgExecutor<'_>, id: Uuid, reason: &str) -> sqlx::Result<()>;
  /// Transient: reschedule with backoff — unless `attempts >= max_attempts`, in which case this
  /// calls `fail` instead. The claim predicate stops offering such a row, so nothing else would
  /// ever write `failed_at` and the row would sit unexplained for ever.
  pub async fn defer(db: impl PgExecutor<'_>, j: &Claimed, reason: &str) -> sqlx::Result<()>;
  ```

Backoff: `30 * 2^(attempts-1)` seconds, capped at an hour, with jitter.

- [ ] **Step 1: Write the failing tests** in `tests/worker_claim.rs`:
  - `two_workers_racing_one_row_claim_it_once` — **invariant 1**: insert one job, run `claim`
    concurrently on two connections, assert the two result vectors' total length is exactly 1.
  - `a_claimed_row_is_not_re_offered_before_its_lease_expires`, and **is** offered after it.
  - `a_completed_row_is_never_re_claimed` — the defect `0025`'s comment describes.
  - `a_row_at_max_attempts_is_not_claimed`.
  - `claiming_filters_by_kind` — a row of another kind is left alone (spec §5.3).
  - `defer_at_the_last_attempt_writes_failed_at` and a `last_error`, rather than leaving the row
    silently unclaimable.
  - `nothing_deletes_a_jobs_row`: after a full lifecycle (claim → complete, and claim → fail), the
    rows are still present.
- [ ] **Step 2: Run and watch them fail.**
- [ ] **Step 3: Implement.** Copy the claim statement's shape from the table comment, including
  `for update skip locked`, `run_after` advanced in the **same** statement, `completed_at is null`,
  and `attempts < max_attempts`. Every one is load-bearing and the comment says why.
- [ ] **Step 4: Run the tests and the full gate.** Commit.

---

### Task 10: The `confirmation_email` handler

**Files:**
- Create: `src/worker/confirmation.rs`
- Modify: `src/worker/mod.rs`
- Test: `tests/worker_confirmation.rs`

**Interfaces:**
- Consumes: Tasks 3–9. `suppressions::blocking_suppression`, `public::token`'s `Purpose::Confirm`
  14-day TTL, `config::Config::public_url`.
- Produces: `pub async fn handle(ctx: &Ctx, job: &Claimed) -> Outcome` plus whatever `Ctx` needs to
  carry (pool, secrets, config, `Transports`).

The sequence, in order:

1. Parse `payload` into `{ email, token }`. A payload that does not parse is `fail`, not a panic.
2. **Stale check**: if `job.created_at` is older than the `Confirm` TTL (14 days), `fail` with a
   reason. The link is already dead and mailing it would send someone a `link-expired` page — the
   table comment states this.
3. **Send-time eligibility**: `blocking_suppression(db, secrets, job.workspace_id, &email)`. A block
   means `fail` with the reason, not a send. **Not `mailable_list_members`** — the view matches
   plaintext and is blind to erasure tombstones, which keep a hash and no plaintext (spec §5.7).
4. Resolve the sending domain for the workspace, then the relays in failover order.
5. Build with `mail::build`, sign with `mail::dkim::signed_bytes` when the domain is `app`-signed —
   otherwise take `lettre::Message::formatted()` — and submit with `Transports::send`.
6. Record: `Sent` → `complete`; `Transient` → `defer`; `Failed` and `Unknown` → `fail`, with the
   reason. **`Unknown` is never retried** (§5.8).

**The handler does not touch the token bucket.** The bucket is debited once per claimed batch
(spec §5.4), which is a property of the tick, not of one message; `handle` runs with the debit
already spent. Task 11's loop owns that call.

- [ ] **Step 1: Write the failing tests:**
  - `a_suppressed_address_is_never_mailed` — **invariant 2**: suppress the address, run the handler
    against a sink that would record any connection, assert the sink saw none and the job failed with
    a suppression reason.
  - `an_erased_contact_is_never_mailed` — the tombstone case: erase the contact (hash kept, plaintext
    gone), assert the same. **This is the test the view could not provide**; assert explicitly that
    `mailable_list_members` would have returned the row, so a future refactor back to the view fails
    here.
  - `a_job_older_than_the_confirm_ttl_fails_and_is_not_sent`.
  - `an_unknown_outcome_is_never_retried` — **invariant 4**: sink drops after `DATA`; assert
    `failed_at` is set, `run_after` was not moved into the future, and a second `claim` returns
    nothing.
  - `a_suppression_reply_is_recorded_as_a_failure` — **invariant 5**.
  - `a_sent_message_carries_our_unsubscribe_link` — **invariant 3**: capture the bytes at the sink,
    assert both headers are present and the URL's origin is the instance's `public_url`.
  - **Review Focus 2:** `a_malformed_payload_fails_that_row_only` — three jobs, the middle one with
    `{"email": 42}`; assert the middle is `failed_at` with a reason, the other two are `completed_at`,
    and the worker made no further claim attempts than the one tick.
- [ ] **Step 2: Run and watch them fail.**
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run the tests and the full gate.** Commit.

---

### Task 11: `nm worker`, `nm all`, and the worker's own pool

**Files:**
- Modify: `src/cli.rs`, `src/main.rs`, `src/config.rs`, `src/db.rs`, `src/worker/mod.rs`,
  `.env.example`, `README.md`
- Test: `tests/worker_pool.rs`, plus `src/config.rs` unit tests

**Interfaces:**
- Produces: `pub async fn worker::run(cfg: Config, shutdown: impl Future<Output = ()>) ->
  anyhow::Result<()>`, and `db::connect_worker(cfg) -> Result<PgPool, sqlx::Error>`.

`Command::Worker` runs the loop alone. `Command::All` runs `serve` and the worker in one process.
**`Command::Serve` does not run the worker** — that is the point of §5.1: an operator who scales the
API does not thereby multiply their send rate, and an API host's pool cannot be drawn down by
sending. New config: `worker_pool_size` (default 5) and `worker_concurrency` (default 4, how many
messages one tick sends at once). `Config`'s hand-written `Debug` destructures without `..`, so both
new fields must be named there — that is deliberate, and it forces a redaction decision.

**The tick has a budget, and it is half the lease.** `BATCH` is 16 and `worker_concurrency` is 4, so
a tick of four waves against a relay that burns the full 60-second `SESSION_TIMEOUT` each wave takes
240 seconds — twice `LEASE_SECS`. A tick that outlives its own lease is the double-send in Review
Focus 1 arriving by arithmetic rather than by a hang. So `run` stops *starting* new sends once the
tick's elapsed time passes `LEASE_SECS / 2`, and explicitly `defer`s every claimed job it did not
start, with a reason. A job that was never attempted is always safe to retry; one that was started
is governed by its own outcome.

- [ ] **Step 1: Write the failing tests:**
  - Config unit tests: both new fields default as above and are overridable by `NM_*`.
  - `config_debug_names_every_field`: already enforced by the destructuring; add the two fields to the
    existing assertions.
  - **Invariant 7**, `tests/worker_pool.rs`: build an API pool and a worker pool from one config;
    saturate the worker pool with `worker_pool_size` held connections; assert the API pool still
    hands out a connection and `/readyz` answers `200`. Without separate pools this fails.
  - `the_tick_drains_a_batch_and_stops_on_shutdown`: enqueue `BATCH + 3` jobs against a sink that
    replies `250`; run `worker::run` with a shutdown future that resolves after two ticks; assert
    exactly `BATCH` rows completed on the first tick and the remaining 3 on the second, and that
    `run` returns rather than being cancelled.
  - `an_exhausted_bucket_sends_nothing_this_tick_and_leaves_the_rows_claimable`: set the relay's
    bucket to zero; assert no connection reached the sink, and that the claimed rows carry a
    `run_after` in the future (deferred) rather than `failed_at` — a throttle is not a failure.
- [ ] **Step 2: Run and watch them fail.**
- [ ] **Step 3: Implement.** `connect_worker` takes `worker_pool_size`; `db::connect` keeps its 20.
  Both honour the same shutdown signal `app::serve` already installs.
- [ ] **Step 4: Document** the three modes in `README.md` and the new variables in `.env.example`.
- [ ] **Step 5: Run the tests and the full gate.** Commit.

---

### Task 12: The settings screens

**Files:**
- Create: `web/src/routes/Relays.tsx`, `web/src/routes/SendingDomains.tsx`
- Modify: `web/src/router.tsx`, the workspace navigation, `web/src/api/schema.d.ts` (generated)
- Test: whatever the existing route tests in `web/` do; at minimum the bundle gate.

**Interfaces:**
- Consumes: the generated API client from Tasks 3 and 4. Reuse `Dialog`, `ConfirmDialog`, `Select`,
  `Checkbox`, `EmptyState`, `ErrorState`, `Skeleton`, `TableScroller` from `web/src/components` —
  do not write new primitives.

Both routes are lazily loaded, like every other route, so they land in their own chunk under the
50 KB gzipped cap.

The sending-domain screen shows each check's state with its failure reason, the `openssl` command
for generating a key (D4), the `v=DKIM1; k=rsa; p=…` value to publish once a key is saved, and a
**Re-check** button. Sending from a domain whose checks are not all `Pass` requires an explicit
acknowledgement — a checkbox the operator ticks, stored on the domain — and the screen says what is
likely to happen without it (spam folder, or rejection).

The relay screen never displays a password; it shows "set" or "not set" with a control to replace or
clear it.

- [ ] **Step 1: Build the two routes**, following the structure of `web/src/routes/Suppressions.tsx`
  for loading, error and empty states.
- [ ] **Step 2: Add them to the router and the navigation**, lazily.
- [ ] **Step 3: Run** `pnpm --dir web build`, `check:bundle`, `check:licenses`. The initial chunk must
  still be under 150 KB gzipped; each new chunk under 50 KB.
- [ ] **Step 4: Run the full gate.** Commit.

---

## Acceptance, and the one thing this plan cannot do

Spec §8.2: a double opt-in signup through an M1d form produces a confirmation email that arrives,
whose link confirms the subscription, whose `List-Unsubscribe` is ours and works, and whose DKIM
signature covers it.

**The live DKIM test (spec §8.1) needs the operator's real OCI tenancy and cannot be run here.** It
gates M2's sign-off, not this plan's tasks: the five questions it answers (does OCI's `h=` cover the
unsubscribe headers; are client headers signed at all; does OCI keep our HTTPS `List-Unsubscribe`;
does a signature we apply survive; which `d=` appears) decide the *default* signing mode, and Task 6
builds both modes either way. Record the answers in the spec when they arrive. If questions 1 and 2
come back "no", app-signing stops being a switch and becomes the only supported mode for that relay
— a one-line default change, not a rework.
