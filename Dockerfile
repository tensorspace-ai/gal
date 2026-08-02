# Build a static-ish single binary. SQLite is compiled from source by
# libsqlite3-sys, so the builder needs a C toolchain; the runtime image does not.
FROM rust:1.97-bookworm AS build
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY crates ./crates
RUN cargo build --release -p gal-server

FROM debian:bookworm-slim
RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates \
 && rm -rf /var/lib/apt/lists/* \
 && useradd --system --create-home --uid 10001 gal
COPY --from=build /src/target/release/gal-server /usr/local/bin/gal-server

USER gal
WORKDIR /data
# The database lives here; mount a volume so it survives container replacement.
VOLUME ["/data"]
ENV GAL_HOST=0.0.0.0 \
    GAL_PORT=8080 \
    GAL_DB=/data/gal.db
EXPOSE 8080
# /healthz touches the database, so it fails when the server cannot serve.
HEALTHCHECK --interval=30s --timeout=3s --start-period=5s \
  CMD ["/usr/local/bin/gal-server", "--healthcheck"]
ENTRYPOINT ["/usr/local/bin/gal-server"]
