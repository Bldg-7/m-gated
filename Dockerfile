FROM node:22.22.2-bookworm-slim AS dashboard
WORKDIR /build/web
COPY web/package.json web/package-lock.json web/.npmrc ./
RUN npm ci --ignore-scripts --no-audit --no-fund
COPY web/ ./
RUN npm run build && test -f dist/.vite/manifest.json && ! find dist -name '*.map' | grep .

FROM rust:1.95.0-bookworm AS binary
WORKDIR /build
COPY Cargo.toml Cargo.lock rust-toolchain.toml ./
COPY src/ ./src/
COPY crates/ ./crates/
COPY adapters/ ./adapters/
COPY migrations/ ./migrations/
RUN cargo build --locked --release

# DeepSeek WASM 어댑터(결정 0009). 설정에 `adapter`를 적은 경우에만 쓴다. 릴리스 워크플로는
# Linux amd64에서 한 번 빌드한 파일을 `--build-context adapter=<디렉터리>`로 주어 이 단계를 바꾼다.
# 그래서 모든 아키텍처의 이미지가 같은 파일을 담는다.
FROM rust:1.95.0-bookworm AS adapter-build
WORKDIR /build
RUN rustup target add wasm32-unknown-unknown \
    && cargo install --locked --quiet --root /build/target/tools wasm-tools@1.259.0
COPY Cargo.toml Cargo.lock rust-toolchain.toml ./
COPY src/ ./src/
COPY crates/ ./crates/
COPY adapters/ ./adapters/
COPY scripts/build-adapter.sh ./scripts/
RUN scripts/build-adapter.sh

FROM scratch AS adapter
COPY --from=adapter-build /build/target/adapters/deepseek.wasm /build/target/adapters/deepseek.wasm.sha256 /

FROM debian:bookworm-slim AS runtime
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/*
WORKDIR /app
COPY --from=binary --chown=65532:65532 /build/target/release/m-gated /app/m-gated
COPY --from=dashboard --chown=65532:65532 /build/web/dist/ /app/web/dist/
COPY --from=adapter /deepseek.wasm /deepseek.wasm.sha256 /app/adapters/
USER 65532:65532
EXPOSE 4000
STOPSIGNAL SIGTERM
ENTRYPOINT ["/app/m-gated"]
