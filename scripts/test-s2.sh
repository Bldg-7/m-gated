#!/usr/bin/env bash
# 전용 로컬 PG와 합성 loopback fixture만 사용한다. 실제 자격/회사 서비스는 필요하지 않다.
set -euo pipefail
cd "$(dirname "$0")/.."
: "${TEST_DATABASE_URL:?명시 TEST_DATABASE_URL이 필요합니다. 제품 DB fallback은 없습니다.}"
: "${TEST_FNM_DIR:?Node22.22.2가 설치된 명시 TEST_FNM_DIR이 필요합니다.}"
fnm_binary="$(command -v fnm)"
public_home="$(mktemp -d /tmp/m-gated-s2-npm.XXXXXX)"
trap 'rm -rf "$public_home"' EXIT
: > "$public_home/user-npmrc"
: > "$public_home/global-npmrc"
env -i PATH="$(dirname "$fnm_binary"):/usr/bin:/bin" HOME="$public_home" \
  FNM_DIR="$TEST_FNM_DIR" NPM_CONFIG_USERCONFIG="$public_home/user-npmrc" \
  NPM_CONFIG_GLOBALCONFIG="$public_home/global-npmrc" DO_NOT_TRACK=1 \
  "$fnm_binary" exec --using 22.22.2 npm --prefix tests/compat ci \
  --ignore-scripts --no-audit --no-fund --registry=https://registry.npmjs.org
cargo test --locked
cargo test --locked --test auth_postgres -- --ignored
# 실제 분 경계 시험은 내부 두 task/row lock은 유지하되 다른 suite 부하와 분리해 반드시 실행한다.
clock_test=s3_contract::saturated_previous_minute_retries_after_authoritative_anchor_for_both_lanes
migration_test=s10::two_migrators_serialize_and_actual_advisory30s_total120s_fail_closed
cleanup_test=s10::pool_initialization_failure_cleans_only_owned_schema_and_marker
cargo test --locked --test postgres_contract -- --ignored --skip "$clock_test" --skip "$migration_test" --skip "$cleanup_test" --skip s10::telemetry_ --skip s10_process::recovery_process_worker
cargo test --locked --test postgres_contract "$clock_test" -- --ignored --exact --test-threads=1 --nocapture
cargo test --locked --test postgres_contract "$migration_test" -- --ignored --exact --test-threads=1 --nocapture
cargo test --locked --test postgres_contract "$cleanup_test" -- --ignored --exact --test-threads=1
# DB 범위 절대 경보누계는 다른 자기 schema의 process-global 계측과 섞지 않는다.
for alarm_test in telemetry_actual_sql_jwks_provider_success_unknown_and_company_key_are_redacted telemetry_pricing_alarm_follows_durable_pause_and_replay_does_not_duplicate telemetry_final_commit_ack_loss_reobserves_durable_alarm_once_after_reassembly telemetry_handoff_import_baseline_is_atomic_once_through_ack_loss_and_reassembly telemetry_handoff_retained_settlement_and_released_safety_keep_baseline_not_active_gauges; do
  cargo test --locked --test postgres_contract "s10::$alarm_test" -- --ignored --exact --test-threads=1
 done
cargo fmt --all -- --check
cargo clippy --locked --all-targets -- -D warnings
git diff --check
