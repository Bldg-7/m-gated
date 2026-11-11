#!/usr/bin/env bash
# S5 키·관리·금융 원자성과 앞단계의 전체 로컬 회귀다.
set -euo pipefail
cd "$(dirname "$0")/.."
: "${TEST_DATABASE_URL:?전용 TEST_DATABASE_URL 필요}"
: "${TEST_FNM_DIR:?고정 Node22.22.2 설치 경로 필요}"
: "${TEST_PLAYWRIGHT_BROWSERS_PATH:?전용 Playwright cache 필요}"
test "$TEST_DATABASE_URL" = 'postgres://m_gated_test@127.0.0.1:64603/m_gated_test'
cargo test --locked --test management_postgres -- --ignored
./scripts/test-s4.sh
printf 'S5_CUMULATIVE_KEYS_ADMIN_ATOMICITY_AND_ACTUAL_MAIN_OK\n'
