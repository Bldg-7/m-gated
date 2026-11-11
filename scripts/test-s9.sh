#!/usr/bin/env bash
# 전 오류 원인·고정 SDK·안전 CLI 음성 profile·필수 브라우저·실제 main 누적 회귀다.
set -euo pipefail
cd "$(dirname "$0")/.."
: "${TEST_DATABASE_URL:?전용 TEST_DATABASE_URL 필요}"
: "${TEST_FNM_DIR:?고정 Node22.22.2 설치 경로 필요}"
: "${TEST_PLAYWRIGHT_BROWSERS_PATH:?전용 Playwright cache 필요}"
test "$TEST_DATABASE_URL" = 'postgres://m_gated_test@127.0.0.1:64603/m_gated_test'
# S8 누적은 새 S9 PG/SDK/browser 및 실제120초 read-idle 시험까지 모든 target을 실행한다.
./scripts/test-s8.sh
cargo fmt --all -- --check
cargo clippy --locked --all-targets -- -D warnings
git diff --check
test -z "$(git diff --cached --name-only)"
test -z "$(find target/test-owned-schemas -type f -print -quit)"
printf 'S9_CUMULATIVE_ERROR_SDK_BROWSER_AND_ACTUAL_MAIN_OK\n'
