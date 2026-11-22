# NM

Open-source (AGPL-3.0), self-hosted email marketing platform in Rust and Postgres.

## Quick start

Everything runs in a Linux dev container, so Docker and a POSIX shell are all the host needs:
every `cargo`, `sqlx`, `cargo deny` and `pnpm` command goes through `bash scripts/dev.sh …`.

**Docker Compose 2.24 or newer** (`docker compose version`). `docker-compose.dev.yml` uses the
long `env_file` syntax so that `.env` is optional — which is what lets the test suite run in a
clone that has never created one — and an older Compose rejects it at step 1.

From a fresh clone to a signed-in browser, in order:

```sh
# 1. Postgres.
docker compose -f docker-compose.dev.yml -p nm up -d

# 2. `.env`, with a secret key. Compose loads this file into the dev container; without
#    NM_SECRET_KEY the server exits before it binds a port.
cp .env.example .env
printf 'NM_SECRET_KEY=%s\n' "$(bash scripts/dev.sh cargo run -q -- keys generate | tr -d '\r')" >> .env

# 3. Schema.
bash scripts/dev.sh cargo run -q -- migrate

# 4. The web UI. Without `web/dist` every non-API path answers `503 web UI not built`.
bash scripts/dev.sh pnpm --dir web install
bash scripts/dev.sh pnpm --dir web build

# 5. An account to sign in with. The password is the first line of stdin.
echo 'correct horse battery staple' | bash scripts/dev.sh cargo run -q -- \
  admin create-user --email you@example.com --instance-admin

# 6. Serve on http://localhost:8080 and sign in with the address and password above.
bash scripts/dev-serve.sh cargo run -q -- serve
```

## The three run modes

Serving and sending are separate processes, and **`nm serve` does not send**:

| command | HTTP API | sends queued mail |
| --- | --- | --- |
| `nm serve` (the default when no subcommand is given) | yes | no |
| `nm worker` | no | yes |
| `nm all` | yes | yes |

That split is the point. Scaling the API means more `nm serve` replicas, and it must not thereby
multiply the send rate — four API replicas and one worker send at one worker's rate. The worker
also has a Postgres pool of its own, `NM_WORKER_POOL_SIZE` (default 5), because `/readyz` reads an
exhausted pool as not ready: sending drawing on the API's twenty connections would let a batch of
mail take the API down. `NM_WORKER_CONCURRENCY` (default 4) is how many messages one tick sends at
once; a tick claims sixteen and stops starting new sends once it has spent half the job lease, so
it can never outlive the claim it is working under.

**Sending needs `NM_PUBLIC_URL` to be `https`.** Every message carries a one-click unsubscribe
link built from it, and a non-`https` one is refused — so on the default `http://localhost:8080`
the worker refuses every confirmation email. Both worker modes say so at startup rather than once
per message: a warning on localhost, and a refusal to start on any host subscribers could reach.

## Development

```sh
bash scripts/dev.sh cargo test        # the full Rust suite
bash scripts/dev.sh cargo clippy --all-targets -- -D warnings
bash scripts/dev.sh cargo fmt --check
bash scripts/dev.sh cargo deny check
bash scripts/check-spdx.sh
```

## Domain vocabulary and decisions

`GLOSSARY.md` defines the domain terms (contact, list, suppression, eligible, and so on); use them as
written. `docs/adr/` records the decisions that are expensive to reverse, each with the alternative
it rejected and what it costs if wrong.

## Web UI

The React UI in `web/` is compiled into the binary from `web/dist`, so `cargo build` after
`pnpm --dir web build` ships one self-contained server. Until that build exists, every non-API
path answers `503 web UI not built; run pnpm --dir web build`.

Node and pnpm live in the dev container too:

- `bash scripts/dev.sh pnpm --dir web install`
- `bash scripts/dev.sh pnpm --dir web build` — typecheck, bundle, then write the `.br`/`.gz`
  copies the server serves
- `bash scripts/dev.sh pnpm --dir web check:bundle` — initial JS must stay under 150 KB gzipped, and
  every other JS chunk (each lazily loaded route) under 50 KB gzipped on its own
- `bash scripts/dev.sh pnpm --dir web check:licenses` — production dependency licenses
- `bash scripts/dev.sh pnpm --dir web gen:api` — regenerate **both** `src/api/openapi.json`
  (from the server) and `src/api/schema.d.ts` (from that JSON). Run this, not half of it: CI
  fails if either has drifted.
- `bash scripts/dev.sh pnpm --dir web gen:schema` — the second half on its own, for when only
  `openapi.json` changed. `gen:api` calls it, so there is one copy of the generation logic

For the Vite dev server (`pnpm --dir web dev`, port 5173) run the backend with
`NM_PUBLIC_URL=http://localhost:5173`, so its session cookie matches the origin the browser sees.
