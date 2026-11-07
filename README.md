# NM

Open-source (AGPL-3.0), self-hosted email marketing platform in Rust and Postgres.

## Development

Rust builds run in a Linux dev container, so every `cargo`, `sqlx` and `cargo deny` command goes through `bash scripts/dev.sh …`.

1. Start Postgres: `docker compose -f docker-compose.dev.yml -p nm up -d`
2. Copy the environment file: `cp .env.example .env`
3. Run the tests: `bash scripts/dev.sh cargo test`

To serve on port 8080, use `bash scripts/dev-serve.sh cargo run -- serve`.
