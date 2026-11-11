#!/usr/bin/env bash
# Responses와 선행 PG/SDK/UI/브라우저·최신 release main TLS를 함께 검증한다.
set -euo pipefail
cd "$(dirname "$0")/.."
: "${TEST_DATABASE_URL:?전용 TEST_DATABASE_URL 필요}"
: "${TEST_FNM_DIR:?고정 Node22.22.2 경로 필요}"
: "${TEST_PLAYWRIGHT_BROWSERS_PATH:?전용 Playwright cache 필요}"
test "$TEST_DATABASE_URL" = 'postgres://m_gated_test@127.0.0.1:64603/m_gated_test'
./scripts/test-u2.sh
cargo test --locked --test responses_postgres -- --ignored --nocapture
cargo fmt --all -- --check
cargo clippy --locked --all-targets -- -D warnings
git diff --check
test -z "$(git diff --cached --name-only)"
test -z "$(find target/test-owned-schemas -type f -print -quit)"
printf 'S6_CUMULATIVE_RESPONSES_PG_SDK_UI_AND_ACTUAL_MAIN_OK\n'
