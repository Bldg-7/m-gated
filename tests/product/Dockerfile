FROM node:22.22.2-bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends openssl ca-certificates && rm -rf /var/lib/apt/lists/*
WORKDIR /fixture
COPY tests/process/package*.json tests/process/.npmrc ./
RUN npm ci --ignore-scripts --no-audit --no-fund
COPY tests/product/*.mjs ./
COPY src/telemetry.rs ./telemetry.rs
COPY tests/process/load.mjs ./load.mjs
CMD ["node","support.mjs"]
