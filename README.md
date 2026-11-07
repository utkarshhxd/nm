# NM

Open-source (AGPL-3.0), self-hosted email marketing platform in Rust and Postgres.

## Quick start

Everything runs in a Linux dev container, so Docker and a POSIX shell are all the host needs:
every `cargo`, `sqlx`, `cargo deny` and `pnpm` command goes through `bash scripts/dev.sh …`.
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

## Development

```sh
bash scripts/dev.sh cargo test        # the full Rust suite
bash scripts/dev.sh cargo clippy --all-targets -- -D warnings
bash scripts/dev.sh cargo fmt --check
bash scripts/dev.sh cargo deny check
bash scripts/check-spdx.sh
```

## Web UI

The React UI in `web/` is compiled into the binary from `web/dist`, so `cargo build` after
`pnpm --dir web build` ships one self-contained server. Until that build exists, every non-API
path answers `503 web UI not built; run pnpm --dir web build`.

Node and pnpm live in the dev container too:

- `bash scripts/dev.sh pnpm --dir web install`
- `bash scripts/dev.sh pnpm --dir web build` — typecheck, bundle, then write the `.br`/`.gz`
  copies the server serves
- `bash scripts/dev.sh pnpm --dir web check:bundle` — initial JS must stay under 150 KB gzipped
- `bash scripts/dev.sh pnpm --dir web check:licenses` — production dependency licenses
- `bash scripts/dev.sh pnpm --dir web gen:api` — regenerate **both** `src/api/openapi.json`
  (from the server) and `src/api/schema.d.ts` (from that JSON). Run this, not half of it: CI
  fails if either has drifted.
- `bash scripts/dev.sh pnpm --dir web gen:schema` — the second half on its own, for when only
  `openapi.json` changed. `gen:api` calls it, so there is one copy of the generation logic

For the Vite dev server (`pnpm --dir web dev`, port 5173) run the backend with
`NM_PUBLIC_URL=http://localhost:5173`, so its session cookie matches the origin the browser sees.
