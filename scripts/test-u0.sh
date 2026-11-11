#!/usr/bin/env bash
# 고정 Node와 전용 PG/브라우저만 사용하는 누적 U0 회귀다.
set -euo pipefail
cd "$(dirname "$0")/.."
: "${TEST_DATABASE_URL:?전용 loopback TEST_DATABASE_URL을 명시해야 합니다.}"
: "${TEST_FNM_DIR:?Node22.22.2가 설치된 TEST_FNM_DIR을 명시해야 합니다.}"
: "${TEST_PLAYWRIGHT_BROWSERS_PATH:?고정 Playwright의 전용 cache를 명시해야 합니다.}"
export TEST_FNM_BINARY="$(command -v fnm)"
home="$(mktemp -d /tmp/m-gated-u0-npm.XXXXXX)"
trap 'rm -rf "$home"' EXIT
: > "$home/user-npmrc"
: > "$home/global-npmrc"
node_command() {
  env -i PATH="$(dirname "$TEST_FNM_BINARY"):/usr/bin:/bin" HOME="$home" \
    FNM_DIR="$TEST_FNM_DIR" NPM_CONFIG_USERCONFIG="$home/user-npmrc" \
    NPM_CONFIG_GLOBALCONFIG="$home/global-npmrc" DO_NOT_TRACK=1 \
    "$TEST_FNM_BINARY" exec --using 22.22.2 "$@"
}
node_command npm --prefix web ci --ignore-scripts --no-audit --no-fund
node_command npm --prefix web run typecheck
node_command npm --prefix web test
node_command npm --prefix web run build
./scripts/test-s2.sh
cargo test --locked --test dashboard_postgres -- --ignored
cargo test --locked --test dashboard_e2e -- --ignored --nocapture
cargo fmt --all -- --check
cargo clippy --locked --all-targets -- -D warnings
git diff --check
test -z "$(git diff --cached --name-only)"
test -z "$(find target/test-owned-schemas -type f -print -quit)"
printf 'U0_CUMULATIVE_REGRESSION_OK\n'
