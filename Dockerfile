FROM node:22.22.2-bookworm-slim AS dashboard
WORKDIR /build/web
COPY web/package.json web/package-lock.json web/.npmrc ./
RUN npm ci --ignore-scripts --no-audit --no-fund
COPY web/ ./
RUN npm run build && test -f dist/.vite/manifest.json && ! find dist -name '*.map' | grep .

FROM rust:1.93.0-bookworm AS binary
WORKDIR /build
COPY Cargo.toml Cargo.lock rust-toolchain.toml ./
COPY src/ ./src/
COPY migrations/ ./migrations/
RUN cargo build --locked --release

FROM debian:bookworm-slim AS runtime
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/*
WORKDIR /app
COPY --from=binary --chown=65532:65532 /build/target/release/m-gated /app/m-gated
COPY --from=dashboard --chown=65532:65532 /build/web/dist/ /app/web/dist/
USER 65532:65532
EXPOSE 4000
STOPSIGNAL SIGTERM
ENTRYPOINT ["/app/m-gated"]
