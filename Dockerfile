# Build stage: static musl release binary. rust:alpine targets musl natively
# and ships gcc plus musl-dev, which the bundled C code in libsqlite3-sys
# compiles against. reqwest uses rustls (default-features = false), so no
# OpenSSL enters the build.
FROM rust:1.97.1-alpine AS build
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

# Runtime stage: Alpine with the static binary and no extra packages.
# Migrations are embedded in the binary by sqlx::migrate! at compile time, so
# only the executable is copied. Upstream HTTPS calls trust the roots that
# rustls (webpki-roots) compiled into the binary, so ca-certificates is
# deliberately not installed, and the healthcheck uses the busybox wget that
# Alpine already ships instead of a curl package.
FROM alpine:3 AS runtime
# busybox adduser does not create a same-name group for a system user, so the
# group comes first; the login shell is disabled like the Debian variant.
RUN addgroup -S hydra \
    && adduser -S -D -H -h /data -s /sbin/nologin -G hydra hydra \
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
    CMD wget -q -O /dev/null http://127.0.0.1:8080/admin/health || exit 1

ENTRYPOINT ["/usr/local/bin/hydra"]
