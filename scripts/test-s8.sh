#!/usr/bin/env bash
# DeepSeek 독립계약과 선행 API/PG/SDK/웹/실제 release main 전체 회귀다.
set -euo pipefail
cd "$(dirname "$0")/.."
: "${TEST_DATABASE_URL:?전용 TEST_DATABASE_URL 필요}"
: "${TEST_FNM_DIR:?고정 Node22.22.2 경로 필요}"
: "${TEST_PLAYWRIGHT_BROWSERS_PATH:?전용 Playwright cache 필요}"
test "$TEST_DATABASE_URL" = 'postgres://m_gated_test@127.0.0.1:64603/m_gated_test'
./scripts/test-s7.sh
# 각 시험의 내부 경합은 유지하고, 독립 fixture들의 초기 pool 연결만 유한하게 스케줄한다.
cargo test --locked --test deepseek_postgres -- --ignored --nocapture --test-threads=2
cargo fmt --all -- --check
cargo clippy --locked --all-targets -- -D warnings
git diff --check
test -z "$(git diff --cached --name-only)"
test -z "$(find target/test-owned-schemas -type f -print -quit)"
printf 'S8_CUMULATIVE_DEEPSEEK_PG_SDK_UI_AND_ACTUAL_MAIN_OK\n'
