FROM rust:1.96.0-bookworm
RUN rustup component add rustfmt clippy \
 && cargo install sqlx-cli --no-default-features --features rustls,postgres --locked \
 && cargo install cargo-deny --locked
WORKDIR /work
