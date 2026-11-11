#!/usr/bin/env bash
# U2 관리자 UI와 선행 단계의 PG/SDK/브라우저/실제 main 누적 회귀다.
set -euo pipefail
cd "$(dirname "$0")/.."
: "${TEST_DATABASE_URL:?전용 TEST_DATABASE_URL 필요}"
: "${TEST_FNM_DIR:?고정 Node22.22.2 설치 경로 필요}"
: "${TEST_PLAYWRIGHT_BROWSERS_PATH:?전용 Playwright cache 필요}"
test "$TEST_DATABASE_URL" = 'postgres://m_gated_test@127.0.0.1:64603/m_gated_test'
export TEST_FNM_BINARY="$(command -v fnm)"
./scripts/test-u1.sh
cargo test --locked --test admin_e2e -- --ignored --nocapture
cargo fmt --all -- --check
cargo clippy --locked --all-targets -- -D warnings
git diff --check
test -z "$(git diff --cached --name-only)"
test -z "$(find target/test-owned-schemas -type f -print -quit)"
printf 'U2_CUMULATIVE_ADMIN_UI_AND_ACTUAL_MAIN_OK\n'
