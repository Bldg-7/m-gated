FROM rust:1.93.0-bookworm AS build
WORKDIR /build
COPY Cargo.toml Cargo.lock rust-toolchain.toml ./
COPY src ./src
COPY migrations ./migrations
RUN cargo build --locked --release
FROM node:22.22.2-bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends openssl ca-certificates && rm -rf /var/lib/apt/lists/*
WORKDIR /fixture
COPY --from=build /build/target/release/m-gated /fixture/m-gated
COPY web/dist /fixture/web/dist
COPY tests/process/package*.json tests/process/.npmrc /fixture/tests/process/
RUN npm --prefix tests/process ci --ignore-scripts --no-audit --no-fund
COPY tests/process/main.mjs tests/process/load.mjs tests/process/legacy-writer.mjs /fixture/tests/process/
COPY tests/support/config.yaml /fixture/synthetic-config.yaml
CMD ["node","tests/process/main.mjs"]
