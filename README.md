# NM

Open-source (AGPL-3.0), self-hosted email marketing platform in Rust and Postgres.

## Development

1. Start Postgres: `docker compose -f docker-compose.dev.yml -p nm up -d`
2. Copy the environment file: `cp .env.example .env`
3. Run the tests: `cargo test`
