FROM rust:1.96.0-bookworm
RUN rustup component add rustfmt clippy \
 && cargo install sqlx-cli --version 0.9.0 --no-default-features --features rustls,postgres --locked \
 && cargo install cargo-deny --locked
WORKDIR /work
