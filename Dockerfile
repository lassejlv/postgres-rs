# syntax=docker/dockerfile:1

FROM rust:1.96.0-slim-bookworm AS builder

WORKDIR /build
COPY . .

RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/build/target \
    cargo build --release --bin postgres-rs && \
    cp target/release/postgres-rs /usr/local/bin/postgres-rs

FROM debian:bookworm-slim AS runtime

RUN useradd --system --uid 10001 --create-home --home-dir /home/pgrs pgrs && \
    mkdir -p /data && chown pgrs:pgrs /data

COPY --from=builder /usr/local/bin/postgres-rs /usr/local/bin/postgres-rs

USER pgrs

ENV PGRS_DATA=/data
VOLUME ["/data"]

EXPOSE 5432

ENTRYPOINT ["postgres-rs"]
CMD ["0.0.0.0:5432"]
