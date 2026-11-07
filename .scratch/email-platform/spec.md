# Email Platform — v1 Design

Status: approved 2026-11-07
Date: 2026-11-07
Working name: **NM** (placeholder, binary `nm`). The real product name gets chosen later.

---

## 1. Intent

We are building an **open-source, self-hosted email marketing platform as a product**. We publish the code. Every user runs it on their own infrastructure with their own sending relay. We never host it for anyone.

**Why.** The reference deployment runs campaigns on self-hosted Interspire Email Marketer 6.2.0 (2018). An audit (2026-11-07) rated it about 330/1000:

- It cannot run on a supported PHP version.
- It lacks the one-click unsubscribe that mailbox providers have required since 2024.
- Its security is weak: md5 password hashes and XOR "encryption".
- It sends through a single-threaded cron job.

We are moving off it. Rather than adopting listmonk or Mautic, we are building a product that combines:

- **listmonk-level speed** and a 5-minute install,
- **Mautic-level automation**,
- **a 2027-quality UI**,
- **built-in deliverability guardrails.** This is the differentiator: the product stops you from hurting your sender reputation before it happens.

**First production user.** The reference deployment sends several hundred thousand emails a day through OCI Email Delivery, and actively uses autoresponders and date-based triggers (e.g. renewal reminders).

## 2. Goals

1. Fully replace Interspire for the reference deployment, including autoresponders and triggers, without losing a single unsubscribe or bounce record in the move.
2. Be fast wherever users feel it: UI, segment counts, imports, tracking redirects and send throughput (targets in §9).
3. Install in under 10 minutes: one binary (or one Docker image) plus Postgres.
4. Be deliverability-safe by default. Every marketing email meets the Gmail / Yahoo / Outlook bulk-sender rules with no configuration.
5. Be API-first. Anything the UI can do, the public API can do.

## 3. Non-goals (v1)

- Hosting, SaaS mode, billing, public sign-ups.
- Running our own mail server (MTA). Users bring a relay: OCI, SES, Postmark, Mailgun, SendGrid, their own Postfix, and so on.
- SMS, WhatsApp, push.
- CRM features (deals, pipelines). No landing-page builder beyond signup and preference pages.
- GraphQL. gRPC only later, and only if a user needs it for one bulk-ingest job.
- Everything in the "Later" lists in §6.

## 4. Anti-goals (failure even if everything "works")

- Sending a campaign to the same contact twice, or emailing anyone who unsubscribed, hard-bounced or complained.
- Becoming Mautic: heavy, slow, and needing expert tuning just to run.
- An install that needs Redis, Elasticsearch, a message broker or a second database.
- A dashboard or segment builder that gets slow at 10M contacts.
- Secrets stored in a reversible or weak form, repeating Interspire's mistakes.

## 5. Decisions and approaches

### Decisions so far

U = decided by the user. C = proposed during design and accepted (or not objected to).

| Decision | By |
|---|---|
| Build our own product: open source, free | U |
| Self-hosted only, never hosted by us | U |
| Speed and efficiency are the top priority; modern 2027 UI; API integrations | U |
| Language: **Rust** | U |
| License: **AGPL-3.0** | U |
| Automation flow builder is in v1 | U (teams use autoresponders and triggers today) |
| Public API: REST + JSON with bulk endpoints, no GraphQL, gRPC only if needed later | C, accepted |
| Database: PostgreSQL only, version 16+ | C, not objected to |
| Feature list in §6 | C, accepted |
| Multiple brands (workspaces) in one install | C, accepted |
| Single binary, bring-your-own relay | C, accepted |
| Design targets: 10× our volume (§9) | C |

### Approaches considered

**Language: Go, Rust or TypeScript (Bun).** Weighted comparison: Rust 90, Go 88. TypeScript was dropped.

- **Go:** about as fast in practice, because the workload mostly waits on SMTP and the database. Faster to build, larger contributor pool, and a mature Postgres job queue (River). Has no native MJML renderer.
- **Rust:** the best reliability. Exhaustive enums suit the many state machines in campaigns and flows, and there are no null or data-race bugs. Lowest memory use, so self-hosting is cheaper. Strong email crates: Stalwart's `mail-auth`, `mail-parser` and `mail-send`, plus `mrml` for MJML. Costs: slower builds, a steeper learning curve, and no mature Postgres queue crate.
- **TypeScript (Bun):** fastest to build but heavier at runtime. Dropped because speed is the top priority.
- **Picked: Rust.** With no time constraint, reliability and efficiency outweigh build speed. The hardest v1 features (DNS/authentication checks, bounce parsing, MJML rendering) already have strong Rust libraries.

**API style: REST, gRPC or GraphQL.**
- REST chosen.
- Serialization is under 1% of request time.
- A single binary makes no internal network calls that gRPC could speed up.
- Every integration speaks HTTP/JSON: n8n, Zapier, curl, PHP, AI agents and webhooks.
- The real efficiency gain comes from bulk endpoints.

**Database: Postgres, MySQL, MongoDB, SQLite, or ClickHouse + Redis.**
- Postgres alone covers relational data, flexible custom fields (JSONB), the job queue (`FOR UPDATE SKIP LOCKED`) and full-text search.
- MySQL has weaker JSON indexing.
- MongoDB's SSPL license is not open source.
- SQLite allows only one writer at a time.
- ClickHouse and Redis would each add a service every self-hoster has to run.
- ClickHouse remains a possible optional store for events on huge installs, Later.

## 6. Features

### v1

**Contacts**
- Contacts with typed custom fields (text, number, date, boolean).
- Lists and tags.
- Live segments: a rule builder with instant counts.
- CSV import with field mapping, de-duplication and validation. Export.
- Suppression list per workspace, plus an optional instance-wide list.
- Double opt-in (on by default for forms). Consent log. Preference center with subscription topics.
- GDPR export and delete per contact.

**Editor**
- Drag-and-drop block editor that produces MJML, plus a raw HTML/MJML mode.
- Templates and a brand kit (logo, colours, fonts, footer) per workspace.
- Merge tags with fallbacks. Conditional blocks.
- Dark-mode preview. Auto-generated plain-text part. Link checker.

**Campaigns**
- Send now or schedule. Optional send at each recipient's local time. Test sends.
- A/B testing of subject, content or sender, with an auto-winner. The default winning metric is **clicks**, because opens are unreliable.
- Rate limits per relay. Multiple relays with failover.
- Pause, resume and cancel at any time.

**Automation**
- A visual flow builder.
- Triggers: joined a list, tag added, a date field (e.g. "30 days before `cert_expiry`"), API event, link clicked, form submitted.
- Steps: wait (a duration, until a date, or until a weekday/time), branch on a condition, send email, add or remove a tag, update a field, call a webhook, exit.
- A re-entry policy per flow.
- Covers everything Interspire's autoresponders and triggers do today.

**Transactional**
- Templated sends through the API (password resets, receipts) on a separate stream from marketing, optionally through a separate relay.
- Reuses the campaign send engine.

**Deliverability ⭐**
- Live DNS checker per sending domain: SPF, DKIM, DMARC, Return-Path alignment.
- RFC 8058 one-click unsubscribe headers on every marketing email.
- Hard bounces and complaints are suppressed automatically.
- Bounce and complaint intake from webhooks (SES, Postmark, Mailgun, SendGrid) and from a mailbox (IMAP/POP3, DSN and ARF parsing).
- **Auto-pause** when the complaint rate goes above 0.3%, or the hard-bounce rate goes above a threshold.
- Bot-click filtering. Opens marked "unreliable" where Apple Mail Privacy Protection is detected.
- Optional DKIM signing inside the app, for relays that don't sign.

**Analytics**
- Per campaign and per flow: delivered, bounced, complained, clicked, unsubscribed, and opened (marked unreliable).
- Link click map. Breakdown by mailbox provider (Gmail, Outlook, Yahoo, other).

**API & integrations**
- Full REST API with an OpenAPI 3.1 spec. API keys scoped per workspace.
- Bulk endpoints.
- Signed outgoing webhooks with retries and a delivery log.
- Embeddable signup forms and a hosted signup page.
- **MCP server**, so AI agents can operate it.

**Team**
- Workspaces (brands). Roles: owner, admin, editor, viewer. An instance-admin role.
- 2FA (TOTP) and passkeys. Audit log.

**Running it**
- Single binary plus Postgres. Docker Compose.
- Config through environment variables.
- Prometheus metrics, with optional OpenTelemetry.
- Automatic migrations.
- **Importers for Interspire and listmonk.**

**UX**
- Command palette (Ctrl/Cmd+K), keyboard-first navigation, dark mode, instant search.
- WCAG 2.2 AA. Works on mobile.

### Later
- **Contacts:** built-in email verification (syntax, MX, disposable domains), contact activity timeline, sunset rules for inactive contacts.
- **Editor:** spam-score check, accessibility checker, multi-language versions.
- **Campaigns:** best-time-to-send, RSS-to-email, an approval step before sending.
- **Automation:** goals and exits, flow analytics, ready-made flow templates.
- **Deliverability:** domain warm-up scheduler, DMARC aggregate-report parsing, Google Postmaster integration, BIMI.
- **Analytics:** revenue and goal tracking.
- **Integrations:** generated SDKs (TypeScript, Python, Go, PHP), n8n / Zapier / Make connectors, HubSpot / Salesforce sync, native HTTP send adapters (e.g. SES v2 API).
- **Team:** SSO (OIDC first, then SAML).
- **AI** (bring your own key: OpenAI, Anthropic or local Ollama): writing assistant, subject-line ideas, building a segment from a sentence, results summaries.
- **Running it:** Helm chart, optional ClickHouse store for events.

## 7. Architecture

### Process model

- **One Rust binary, several modes:**
  - `nm serve`: HTTP API, the embedded web UI, and public endpoints (tracking, unsubscribe, forms).
  - `nm worker`: queue consumers for sending, automation, imports, feedback intake and outgoing webhooks.
  - `nm all` (default): runs both in one process.
  - `nm migrate`, plus admin commands such as `nm admin create-user` and `nm keys rotate`.
- **Postgres is the only external dependency.** The job queue lives in Postgres (`FOR UPDATE SKIP LOCKED`, leases, retries). No Redis.
- **Scaling out:** run more `worker` processes, and more `serve` processes behind a load balancer. Workers coordinate only through Postgres.
- **Media storage:** uploaded images and other email assets go to local disk by default. S3-compatible storage (S3, R2, MinIO) is optional. Apart from these files, Postgres holds all state.
- **Suggested crates** (the builder may substitute): tokio, axum, sqlx (compile-time-checked SQL), lettre and/or mail-send (SMTP), mail-auth (DKIM/SPF/DMARC), mail-parser (DSN/ARF parsing), mrml (MJML), utoipa (OpenAPI), webauthn-rs (passkeys), argon2, rustls, rust-embed (bundled UI).

### Sending pipeline

1. **Launch:** resolve the audience (lists or segment, minus suppressions) into one `messages` row per recipient, unique on `(campaign_id, contact_id)`. Do this in batches, without blocking the UI, and show progress.
2. **Claim:** workers claim batches of queued messages, using leases so a crashed worker's batch is picked up again.
3. **Re-check suppression at send time,** with one lookup per batch. An unsubscribe that lands mid-campaign must take effect for the rest of that campaign.
4. **Render per recipient.** The template is compiled once per campaign, then personalised. Tracked links and unsubscribe tokens are injected.
5. **Send over pooled SMTP connections per relay,** honouring each relay's rate limit (token bucket, shared across workers) and its maximum number of connections. Optional per-recipient-domain caps.
6. **Record results with batched writes.**
   - 4xx responses: retry with backoff.
   - 5xx responses: record as a bounce.
   - **Ambiguous outcome** (connection lost after DATA, before the server replied): mark `unknown` and **do not auto-retry**. Not double-sending matters more than a rare miss.
7. **Pause, resume and cancel** take effect within 5 seconds.
8. **Required headers on every marketing message:**
   - `List-Unsubscribe` (an HTTPS URL, plus an optional `mailto:`)
   - `List-Unsubscribe-Post: List-Unsubscribe=One-Click`
   - `Feedback-ID` (for Gmail's feedback loop)
   - `Message-ID` on the sending domain
   - **Note:** RFC 8058 requires a DKIM signature that covers both List-Unsubscribe headers. Either the relay's signature must cover them, or the app-side DKIM option must be used. Verify this for each supported relay (OCI first).

### Automation engine

- **Flows are versioned graphs** (nodes and edges, stored as JSON). Publishing creates an immutable version. **Runs stay on the version they started on.**
- **`flow_runs`:** one row per contact per enrollment: flow, version, current node, `next_run_at`, state. Due runs are claimed with `SKIP LOCKED` on an index over `next_run_at`.
- **Event triggers** (list joined, tag added, API event, link clicked, form submitted) enrol contacts as the events happen.
- **Date triggers** use a scheduler that, for each flow, regularly finds contacts whose date field plus the offset falls on "today" in the contact's timezone (falling back to the workspace timezone).
- **Each step runs idempotently** under a lease, so a worker crash never skips or repeats a step.
- **Flow sends** go through the same pipeline as campaigns, with the same suppression checks and the same headers.
- **Re-entry policy per flow:** once ever, once per N days, or every time.

### Public endpoints (tracking, unsubscribe, forms)

**URLs and tokens**
- Every public URL uses a compact token signed with HMAC. No sequential IDs appear anywhere public.

**Tracking**
- Click redirect: `GET /t/c/{token}` returns a 302 to the target **without waiting on the database**. Events go into an in-memory buffer that is flushed to the database in batches, at least once a second. Losing a few click events if the process crashes is acceptable.
- Open pixel: `GET /t/o/{token}`, handled the same way.
- Bot filtering flags:
  - clicks that arrive impossibly soon after delivery,
  - clicks from known security-scanner networks or user agents,
  - clicks on hidden honeypot links.
- Apple Mail Privacy Protection opens are flagged as unreliable.

**Unsubscribe**
- `GET /u/{token}` shows a confirmation page and the preference center.
- `POST /u/{token}` with the body `List-Unsubscribe=One-Click` unsubscribes immediately. No login, idempotent, applied in under 5 seconds.

**Tracking domains**
- Each workspace can use its own tracking domain (a CNAME pointing to the install), so links carry the brand's own domain.
- TLS comes from the bundled Caddy using on-demand certificates. Caddy asks the app whether a domain is allowed, and the app answers yes only for verified tracking domains.

### Deliverability internals

**Domain setup** (per workspace): from-domain, DKIM (signed by the relay or by the app), Return-Path, tracking domain. The live DNS checker shows the status of each.
- Sending from a domain that fails its checks requires an explicit acknowledgement.

**Feedback intake**
- Webhook receivers for SES (SNS), Postmark, Mailgun and SendGrid.
- Mailbox polling over IMAP/POP3, parsing DSN bounce messages and ARF complaint reports.

**Classification**
- A hard bounce suppresses the address immediately.
- Soft bounces suppress after N in a row (configurable, default 3).
- A complaint suppresses the address and counts towards the complaint rate.

**Auto-pause**
- Triggers when the complaint rate exceeds 0.3%, or the hard-bounce rate exceeds 5%.
- Applies only after a minimum sample of 1,000 deliveries.
- Applies per campaign and per workspace, and thresholds are configurable.
- Notifies workspace owners by email, in-app and through a webhook event.

**OCI:** the reference deployment uses OCI Email Delivery. Research how OCI reports bounces and complaints (a DSN to a custom Return-Path, Events/Logging, or both), and add an adapter if it needs one (see §13).

### Data model (sketch; the builder owns the final schema)

**Multi-tenancy rule**
- Every tenant table carries `workspace_id`.
- Every query is scoped to a workspace through the data-access layer.
- Tests prove one workspace can never read another's data.

**Core tables**
- `instance_settings`, `users`, `memberships` (user, workspace, role), `workspaces`, `api_keys` (only a hash of the secret is stored)
- `contacts` (email, case-insensitive and unique per workspace; status; `attributes` JSONB; timezone; timestamps), `custom_fields` (definitions and types)
- `lists`, `list_memberships` (subscribed / unconfirmed / unsubscribed, with timestamps), `tags`, `contact_tags`, `segments` (rules as JSON)
- `suppressions` (workspace or instance level, reason, source, timestamp), `consent_log`

**Content and campaigns**
- `templates`, `brand_kits`, `media`
- `campaigns`, `campaign_variants`, `relays` (credentials encrypted), `sending_domains` (DNS check results)

**High-volume tables**
- `messages` (one row per recipient per send): **partitioned by month**
- `events` (delivered / bounced / complained / clicked / opened / unsubscribed): **partitioned by month**
- `campaign_stats`, `flow_stats`: running totals updated as events arrive, so dashboards never scan raw events

**Automation, integrations and system**
- `flows`, `flow_versions`, `flow_runs`
- `jobs` (the queue), `webhook_endpoints`, `webhook_deliveries`
- `audit_log`

**Retention**
- Raw events are kept for 13 months by default (configurable). Running totals are kept forever.

### API

**Basics**
- Base path `/api/v1`, JSON. The OpenAPI 3.1 spec is generated from the code and served at `/api/v1/openapi.json`, with a docs page.
- **The web UI uses only the public API.** That guarantees the API can do everything the UI can.

**Auth**
- API keys: a prefix plus a secret, stored hashed, scoped to one workspace with a set of permissions.
- The UI uses session cookies against the same endpoints.

**Conventions**
- Cursor pagination.
- `Idempotency-Key` on every POST that creates or sends something.
- Errors as RFC 9457 problem details.
- Rate limits per key, reported in `RateLimit` headers.

**Bulk**
- `POST /contacts:batchUpsert` (up to 10k contacts per request).
- Streaming import, NDJSON or CSV.
- `POST /events:batch` for custom events that trigger flows.

**Transactional**
- `POST /tx/send` (template, recipient, data).
- No marketing unsubscribe headers, but hard-bounce suppression still applies.

**Outgoing webhooks**
- Signed per the Standard Webhooks spec (HMAC-SHA256).
- Retried with exponential backoff for up to 24 hours.
- A delivery log with replay.
- Requests to private or link-local addresses are blocked by default (SSRF protection).

**MCP server**
- Endpoint `/mcp` (streamable HTTP), authenticated with an API key.
- Exposes the main operations: search contacts, build a segment, draft a campaign, read stats.
- **Any action that sends email requires an explicit confirmation parameter.**

### UI

**Stack** (design call)
- React + TypeScript + Vite.
- TanStack Router and TanStack Query.
- Radix primitives + Tailwind.
- Built as static files and bundled into the binary.

**Look and feel**
- **To be decided from mockups, with the user, before the frontend work starts** (see §13).
- Must-haves: command palette, keyboard shortcuts, dark mode, optimistic updates, skeleton loading states, WCAG 2.2 AA, responsive layout.

**Frontend performance** (the user asked that React must not cost speed)
- React runs only in the browser, so it adds no load to the server. The binary serves the built files with long-lived cache headers, hashed filenames and precompressed copies (brotli and gzip).
- **The initial app download stays under 150 KB compressed.** The email editor, the flow builder and the charts load only when a user opens them.
- Data is cached with TanStack Query, and the next page's data starts loading when the user hovers over its link.
- Large tables render only the rows on screen (TanStack Virtual). Paging, sorting and searching happen on the server. The browser never receives more than one page of contacts.
- Use React Compiler, or careful memoisation, in the editor and the flow builder.
- The flow builder uses React Flow (xyflow, MIT license).
- The budgets in §9 are enforced in CI: a bundle-size check, plus Lighthouse on key pages.

**Email editor**
- Block-based drag-and-drop that produces MJML, plus a raw mode.
- **The server renders MJML with `mrml`, so the preview always matches what gets sent.**
- Candidate editors: GrapesJS + grapesjs-mjml, easy-email. The license must be compatible with the product license.

**Personalisation**
- Liquid-style syntax, e.g. `{{ first_name | default: "there" }}`, plus conditional blocks.
- Templates are validated on save. An unknown field falls back to its default and never shows raw tags in a sent email.

### Security

**Accounts and sessions**
- Passwords hashed with argon2id.
- Passkeys (WebAuthn) and TOTP 2FA.
- Login rate limiting and lockout.
- Session cookies set `HttpOnly`, `Secure` and `SameSite=Lax`, with CSRF tokens.

**Access**
- Role-based access per workspace. An audit log of security, admin and sending actions.

**Secrets**
- Relay credentials, mailbox passwords and webhook secrets are encrypted with AES-256-GCM using a master key from an environment variable or a file.
- A key-rotation command.
- **Secrets never appear in logs or API responses.**

**Public endpoints**
- Signed tokens only.
- Signup forms use a honeypot and optional ALTCHA proof-of-work: self-hosted, no third party.

**Outgoing HTTP**
- Webhooks and checks block private and link-local addresses by default.

**Response headers**
- CSP, `X-Frame-Options`, `Referrer-Policy`. HSTS comes from the proxy.

**Supply chain**
- `cargo-deny` (licenses and security advisories) and `cargo audit` in CI.
- Reproducible release builds.
- Signed binaries and images (cosign).

### Licensing

- **The product is AGPL-3.0.** Every source file carries an SPDX header, and the repo has a `LICENSE` file.
- **Allowed dependency licenses** (Rust crates and npm packages): MIT, Apache-2.0, BSD-2/3-Clause, ISC, Zlib, MPL-2.0, Unicode-3.0, GPL-3.0 and AGPL-3.0.
- **Never allowed:** GPL-2.0-only, because it cannot be combined with AGPL-3.0. Also never: SSPL, BUSL, Commons Clause, or any other non-OSI "source-available" license.
- CI enforces this with `cargo-deny` for Rust and a license checker for the npm packages.
- **Contributions:** DCO sign-off (`Signed-off-by:`), no CLA. This is a design call. Note that a CLA would be needed if the org ever wants to sell a commercial license alongside the AGPL one.

### Privacy

- **Consent log:** source, IP, timestamp, form or API, and when double opt-in was confirmed.
- **GDPR delete:** erases personal data but keeps a **hashed** email in suppressions, so a returning address is still blocked.
- **Tracking controls:** open tracking and click tracking can each be switched off per workspace and per campaign.

### Ops and distribution

**Packaging**
- Static binaries for Linux x86_64 and arm64 (macOS for development).
- A multi-arch Docker image.
- `docker-compose.yml` with `nm`, `postgres` and `caddy` (automatic TLS, including tracking domains).

**Configuration**
- Environment variables, plus an optional TOML file.
- A "system relay" sends the app's own emails (invites, password resets, alerts).

**Database**
- Postgres 16+.
- Migrations run automatically on start (this can be turned off). `nm migrate` runs them manually.

**Health and monitoring**
- `/healthz` and `/readyz`.
- Prometheus `/metrics`, JSON logs, optional OpenTelemetry tracing.

**Backups**
- Documented backup steps for Postgres (pg_dump / pgBackRest) and the media storage.

**Versioning**
- Semver. No breaking changes within `/api/v1`.

## 8. Correctness invariants (each needs automated tests)

1. A contact receives a given campaign **at most once**. Enforced by a database unique constraint and idempotent workers.
2. No marketing email is sent to a suppressed address (unsubscribed, hard-bounced, complained, or on the instance-wide list). This is checked **at send time**, not only at launch.
3. Every marketing email carries `List-Unsubscribe` and `List-Unsubscribe-Post`, and the one-click endpoint works.
4. One-click unsubscribe is idempotent and needs no login.
5. Pausing a campaign stops new sends within 5 seconds.
6. A worker crash never makes a flow run skip or repeat a step.
7. Secrets never appear in logs or API responses.
8. One workspace can never read or change another workspace's data.

## 9. Performance targets

Targets are 10× our current volume. The reference box is a single VM with 8 vCPU and 32 GB RAM, running both `nm all` and Postgres. A benchmark harness lives in the repo, and a nightly CI run fails when performance regresses.

| Metric | Target |
|---|---|
| Send throughput (to a sink relay, with personalisation and tracking) | ≥ 1,500 emails/sec sustained; scales with workers until the relay or the DB is the limit |
| Daily capacity | 5M emails/day per install |
| Campaign launch, 5M recipients (building the recipient list) | < 5 min, UI stays responsive |
| Click redirect | p99 < 20 ms at 5,000 req/s |
| One-click unsubscribe applied | < 5 s |
| Segment count preview at 10M contacts (common rules) | p95 < 2 s. The exact count may arrive asynchronously, and an estimate while typing is fine. |
| UI API responses (list and detail pages) at target data sizes | p95 < 150 ms server time |
| Import, 1M-row CSV | < 2 min |
| Memory | idle < 100 MB RSS; a worker at full load < 1 GB |
| Install | `docker compose up` to first test email in < 10 min |
| UI initial download (app shell) | < 150 KB compressed JS; editor and flow builder loaded on demand |
| UI first load (warm server, normal broadband) | < 1.5 s to interactive |
| UI page switch | < 100 ms with cached or prefetched data |
| UI responsiveness | INP < 200 ms on every page, including the editor and the flow builder |

## 10. Migration from Interspire (our cutover)

**Command**
- `nm import interspire --mysql-url … --table-prefix email_ --workspace <slug>` reads the Interspire database directly.

**What it maps**
- Lists, contacts and custom fields, subscription status.
- **Unsubscribes and bounces become suppressions.** This is mandatory: re-emailing people who unsubscribed is a legal problem.
- Newsletters and templates come over as raw-HTML templates.
- Autoresponders become flows (list joined → wait → send).
- Date-based triggers become date-triggered flows, wherever they can be mapped.

**Safety**
- Dry-run mode with counts.
- Safe to re-run.
- Produces a report of everything it could not map.

**Cutover, per brand**
1. Import.
2. Verify the counts.
3. Configure the relay (same OCI account) and the DNS checks.
4. Send a small campaign.
5. Move all sending over.

The same relay and domains carry their reputation over, so no warm-up is needed.

A listmonk importer (from its database or API) is also in v1, for adoption by other users.

## 11. Milestones (each ends with something usable)

- **M0 Foundations:** repo, CI, config, migrations, accounts (passwords, passkeys, TOTP), workspaces, roles, API skeleton with OpenAPI, UI shell and design system.
- **M1 Audience:** contacts, custom fields, lists, tags, segment engine, import/export, suppressions, double opt-in, forms, preference center, consent log, GDPR.
- **M2 Sending core:** relays, sending domains with the DNS checker, templates, editor and mrml, personalisation, campaigns (schedule, timezone, test sends, A/B), send pipeline, tracking, one-click unsubscribe.
- **M3 Feedback loop:** bounce and complaint intake (webhooks and mailbox), auto-suppression, auto-pause, bot filtering, analytics and running totals.
- **M4 Automation:** flow builder UI, engine, triggers, steps, versioning.
- **M5 Platform:** API completeness, bulk endpoints, transactional sends, outgoing webhooks, MCP server.
- **M6 Migration:** Interspire and listmonk importers. Our first brand moves over.
- **M7 Hardening and release:** benchmarks meet §9, security review, docs, packaging, v1.0.

**v1 is done when:** the reference deployment has moved over and has sustained its full production volume for 2 weeks in a row with zero violations of §8, and the §9 benchmarks pass.

## 12. Left to the builder

- Final crate choices beyond those suggested in §7.
- Exact schemas and indexes.
- How segment rules compile into SQL.
- Queue details (lease timeouts, batch sizes).
- How rate limits are shared across workers.
- Which editor library to use (license-compatible).
- The template engine (Liquid-compatible preferred).
- Bot-detection and Apple-MPP-detection heuristics.
- Retry and backoff values.
- UI component details within the agreed look and feel.

## 13. Open items

1. ~~License~~: resolved as **AGPL-3.0** (see §5 and the Licensing section in §7).
2. **UI look and feel:** settle it with mockups before the M0 design system is built.
3. **OCI bounce and complaint reporting:** research before M3.
4. **Whether each relay's DKIM signature covers the List-Unsubscribe headers:** verify for OCI first, before M2 is signed off.
5. **Product name:** the codename NM stays until a name is chosen.
6. **Team and who builds it:** needed for the implementation plan, not for this design.
