#!/usr/bin/env bash
# Node 없는 최종 이미지와 독립 cgroup 정상 실행/부하 bridge를 검증한다.
set -euo pipefail
cd "$(dirname "$0")/.."
: "${TEST_DATABASE_URL:?전용 테스트 DB 필요}"
./scripts/test-package.sh
python3 scripts/test-product.py
