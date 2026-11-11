#!/usr/bin/env bash
# 최종 runtime layer만 검사한다. build/test image의 Node 존재와 혼동하지 않는다.
set -euo pipefail
cd "$(dirname "$0")/.."
docker build --label app=m-gated --label purpose=S10-product-package -t m-gated-full-v1-product .
test "$(docker image inspect m-gated-full-v1-product --format '{{.Config.User}}')" = '65532:65532'
docker run --rm --network none --read-only --cap-drop ALL --security-opt no-new-privileges \
  --label app=m-gated --label purpose=S10-product-package \
  --entrypoint /bin/sh m-gated-full-v1-product -eu -c '
  test "$(id -u)" = 65532
  test -x /app/m-gated
  test -f /app/web/dist/.vite/manifest.json
  ! command -v node
  ! command -v npm
  ! command -v cargo
  test -z "$(find /app -name node_modules -o -name \*.map -o -name Cargo.toml -o -name package.json)"
  test ! -e /app/tests
  test ! -e /app/.env
  test ! -e /app/gateway.yaml
  ! touch /tmp/mgated-package-write-test 2>/dev/null
  '
python3 - <<'PY'
import subprocess
r=subprocess.run(['docker','run','--rm','--network','none','--read-only','--cap-drop','ALL','--security-opt','no-new-privileges','--label','app=m-gated','--label','purpose=S10-product-package','m-gated-full-v1-product'],capture_output=True,timeout=10)
assert r.returncode==1 and not r.stdout
assert r.stderr.decode()=='config_input_invalid: 기동 검증에 실패했습니다.\n'
print('S10_NONROOT_NODE_FREE_RUNTIME_AND_FAIL_BEFORE_BIND_OK')
PY
docker image inspect m-gated-full-v1-product --format 'S10_PRODUCT_IMAGE_ID={{.Id}} ARCH={{.Architecture}}'
