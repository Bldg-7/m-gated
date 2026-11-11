#!/usr/bin/env bash
# S3와 선행 단계의 누적 회귀. 회사 endpoint는 합성 TLS loopback에만 고정한다.
set -euo pipefail
cd "$(dirname "$0")/.."
: "${TEST_DATABASE_URL:?전용 TEST_DATABASE_URL 필요}"
: "${TEST_FNM_DIR:?고정 Node22.22.2 설치 경로 필요}"
: "${TEST_PLAYWRIGHT_BROWSERS_PATH:?전용 Playwright cache 필요}"
test "$TEST_DATABASE_URL" = 'postgres://m_gated_test@127.0.0.1:64603/m_gated_test'
./scripts/test-u0.sh
cargo test --locked --test server default_management_ten_seconds_and_asset_thirty_seconds_are_wire_deadlines -- --ignored --exact --nocapture
./scripts/test-main.sh
cargo fmt --all -- --check
cargo clippy --locked --all-targets -- -D warnings
git diff --check
test -z "$(git diff --cached --name-only)"
test -z "$(find target/test-owned-schemas -type f -print -quit)"
printf 'S3_CUMULATIVE_AND_ACTUAL_MAIN_OK\n'
