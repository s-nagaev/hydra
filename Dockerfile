# Build stage: release binary. reqwest uses rustls (default-features =
# false), so no OpenSSL enters the build.
FROM rust:1.97.1-bookworm AS build
WORKDIR /build

# Cache dependencies independently of the source: build the dependency tree
# from a stub first, then copy the real code and rebuild only the crate.
COPY Cargo.toml Cargo.lock ./
RUN mkdir src \
    && echo 'fn main() {}' > src/main.rs \
    && echo '' > src/lib.rs \
    && cargo build --release \
    && rm -rf src
# sqlx::migrate! embeds the migration files (both the SQLite and the
# PostgreSQL set) and askama/include_bytes! embed the templates and static
# assets into the binary at compile time, so all four directories must be
# present in the builder (the runtime image needs only the executable).
COPY src ./src
COPY migrations ./migrations
COPY migrations_pg ./migrations_pg
COPY templates ./templates
COPY assets ./assets
RUN touch src/main.rs src/lib.rs \
    && cargo build --release

# Runtime stage: slim Debian with curl for the HEALTHCHECK. Migrations are
# embedded in the binary by sqlx::migrate! at compile time, so only the
# executable needs to be copied.
FROM debian:bookworm-slim AS runtime
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates curl \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --system --home-dir /data --shell /usr/sbin/nologin hydra \
    && mkdir -p /data \
    && chown hydra:hydra /data

COPY --from=build /build/target/release/hydra /usr/local/bin/hydra

USER hydra
WORKDIR /data
ENV HYDRA_BIND_ADDR=0.0.0.0:8080 \
    HYDRA_DB_PATH=/data/hydra.db
VOLUME /data
EXPOSE 8080

HEALTHCHECK --interval=30s --timeout=3s --start-period=5s --retries=3 \
    CMD curl -fsS http://127.0.0.1:8080/admin/health

ENTRYPOINT ["/usr/local/bin/hydra"]
