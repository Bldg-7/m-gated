#!/usr/bin/env bash
# Chat/Embeddings와 선행 PG/SDK/UI/실제 release main 누적 회귀다.
set -euo pipefail
cd "$(dirname "$0")/.."
: "${TEST_DATABASE_URL:?전용 TEST_DATABASE_URL 필요}"
: "${TEST_FNM_DIR:?고정 Node22.22.2 경로 필요}"
: "${TEST_PLAYWRIGHT_BROWSERS_PATH:?전용 Playwright cache 필요}"
test "$TEST_DATABASE_URL" = 'postgres://m_gated_test@127.0.0.1:64603/m_gated_test'
home="$(mktemp -d /tmp/m-gated-s7-preflight.XXXXXX)"
trap 'rm -rf "$home"' EXIT
: > "$home/user-npmrc"
: > "$home/global-npmrc"
fnm_binary="$(command -v fnm)"
for source in tests/process/main.mjs tests/compat/chat-embeddings.mjs; do
  env -i PATH="$(dirname "$fnm_binary"):/usr/bin:/bin" HOME="$home" FNM_DIR="$TEST_FNM_DIR" \
    NPM_CONFIG_USERCONFIG="$home/user-npmrc" NPM_CONFIG_GLOBALCONFIG="$home/global-npmrc" DO_NOT_TRACK=1 \
    "$fnm_binary" exec --using 22.22.2 node --check "$source"
done
./scripts/test-s6.sh
cargo test --locked --test chat_embeddings_postgres -- --ignored --nocapture
cargo fmt --all -- --check
cargo clippy --locked --all-targets -- -D warnings
git diff --check
test -z "$(git diff --cached --name-only)"
test -z "$(find target/test-owned-schemas -type f -print -quit)"
printf 'S7_CUMULATIVE_CHAT_EMBEDDINGS_PG_SDK_UI_AND_ACTUAL_MAIN_OK\n'
