# syntax=docker/dockerfile:1

FROM rust:1.96.0-slim-bookworm AS builder

WORKDIR /build
COPY . .

RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/build/target \
    cargo build --release --bin postgres-rs && \
    cp target/release/postgres-rs /usr/local/bin/postgres-rs

FROM debian:bookworm-slim AS runtime

# Runs as root: Railway mounts its persistent volume at /data owned by root,
# and an unprivileged user can't write to it.
RUN mkdir -p /data

COPY --from=builder /usr/local/bin/postgres-rs /usr/local/bin/postgres-rs

ENV PGRS_DATA=/data
VOLUME ["/data"]

EXPOSE 5432

ENTRYPOINT ["postgres-rs"]
CMD ["0.0.0.0:5432"]
