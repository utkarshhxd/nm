# NM

Open-source (AGPL-3.0), self-hosted email marketing platform in Rust and Postgres.

## Development

Rust builds run in a Linux dev container, so every `cargo`, `sqlx` and `cargo deny` command goes through `bash scripts/dev.sh …`.

1. Start Postgres: `docker compose -f docker-compose.dev.yml -p nm up -d`
2. Copy the environment file: `cp .env.example .env`
3. Run the tests: `bash scripts/dev.sh cargo test`

To serve on port 8080, use `bash scripts/dev-serve.sh cargo run -- serve`.

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
- `bash scripts/dev.sh pnpm --dir web gen:api` — regenerate `src/api/openapi.json` and
  `src/api/schema.d.ts` from the server's spec; CI fails if the committed spec has drifted

For the Vite dev server (`pnpm --dir web dev`, port 5173) run the backend with
`NM_PUBLIC_URL=http://localhost:5173`, so its session cookie matches the origin the browser sees.
