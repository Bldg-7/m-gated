#!/usr/bin/env bash
# U1 개인키/cookie쓰기와 S5까지의 실제 누적 회귀다.
set -euo pipefail
cd "$(dirname "$0")/.."
: "${TEST_DATABASE_URL:?전용 TEST_DATABASE_URL 필요}"
: "${TEST_FNM_DIR:?고정 Node22.22.2 설치 경로 필요}"
: "${TEST_PLAYWRIGHT_BROWSERS_PATH:?전용 Playwright cache 필요}"
test "$TEST_DATABASE_URL" = 'postgres://m_gated_test@127.0.0.1:64603/m_gated_test'
# 선택진단 enum이 누적 전체 browser를 축소하지 못하도록 한다.
unset TEST_E2E_SCOPE
./scripts/test-s5.sh
printf 'U1_CUMULATIVE_PERSONAL_KEYS_COOKIE_WRITES_AND_ACTUAL_MAIN_OK\n'
