#!/usr/bin/env bash
# S4 금융/count와 앞단계 전체를 실제 PG/SDK/browser/제품 main까지 누적 검증한다.
set -euo pipefail
cd "$(dirname "$0")/.."
: "${TEST_DATABASE_URL:?전용 TEST_DATABASE_URL 필요}"
: "${TEST_FNM_DIR:?고정 Node22.22.2 설치 경로 필요}"
: "${TEST_PLAYWRIGHT_BROWSERS_PATH:?전용 Playwright cache 필요}"
test "$TEST_DATABASE_URL" = 'postgres://m_gated_test@127.0.0.1:64603/m_gated_test'
./scripts/test-s3.sh
printf 'S4_CUMULATIVE_STRICT_COUNT_AND_ACTUAL_MAIN_OK\n'
