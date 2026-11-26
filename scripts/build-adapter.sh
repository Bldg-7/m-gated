#!/usr/bin/env bash
# DeepSeek 어댑터를 WASM 컴포넌트로 빌드한다(결정 0009).
# 결과: target/adapters/deepseek.wasm과, 그 SHA-256을 담은 target/adapters/deepseek.wasm.sha256.
# 마지막 줄에도 SHA-256을 쓴다.
set -euo pipefail
cd "$(dirname "$0")/.."

# wit-bindgen 0.62.0과 같은 날 나온 판이다(wit-component 0.259.0).
WASM_TOOLS_VERSION=1.259.0
TOOLS=target/tools
WASM_TOOLS=$TOOLS/bin/wasm-tools
CORE=target/wasm32-unknown-unknown/release/m_gated_adapter_deepseek_component.wasm
OUT=target/adapters/deepseek.wasm

rustup target add wasm32-unknown-unknown >/dev/null 2>&1
if ! "$WASM_TOOLS" --version 2>/dev/null | grep -q "^wasm-tools $WASM_TOOLS_VERSION\b"; then
  cargo install --locked --quiet --root "$TOOLS" "wasm-tools@$WASM_TOOLS_VERSION"
fi

# 컴포넌트에는 의존 crate의 오류 위치 문자열이 절대 경로로 들어간다. 경로를 고정된 이름으로 바꿔서
# 빌드한 사람의 경로가 남지 않게 하고, 경로가 달라도 같은 파일이 나오게 한다. 빌드하는 컴퓨터의
# 종류(macOS, Linux amd64, Linux arm64)가 다르면 Cargo가 crate마다 붙이는 해시가 달라서, 다른 파일이
# 나온다. 릴리스 이미지는 Linux amd64에서 한 번 빌드한 파일을 쓴다.
export CARGO_TARGET_WASM32_UNKNOWN_UNKNOWN_RUSTFLAGS="--remap-path-prefix=${CARGO_HOME:-$HOME/.cargo}=/cargo --remap-path-prefix=$PWD=/m-gated"
cargo build --locked --release --target wasm32-unknown-unknown -p m-gated-adapter-deepseek-component
mkdir -p "$(dirname "$OUT")"
"$WASM_TOOLS" component new "$CORE" -o "$OUT"
"$WASM_TOOLS" validate "$OUT"

# 컴포넌트는 import를 가질 수 없다. 본체도 시작할 때 다시 검사한다.
WIT=$("$WASM_TOOLS" component wit "$OUT")
if grep -qE '^[[:space:]]*import[[:space:]]' <<<"$WIT"; then
  echo "컴포넌트에 import가 있다:" >&2
  echo "$WIT" >&2
  exit 1
fi
if ! grep -qE '^[[:space:]]*export m-gated:adapter/provider@0\.1\.0;' <<<"$WIT"; then
  echo "컴포넌트가 m-gated:adapter/provider@0.1.0을 내보내지 않는다:" >&2
  echo "$WIT" >&2
  exit 1
fi

if command -v sha256sum >/dev/null; then
  SHA=$(sha256sum "$OUT" | cut -d' ' -f1)
else
  SHA=$(shasum -a 256 "$OUT" | cut -d' ' -f1)
fi
echo "$SHA" >"$OUT.sha256"
echo "$SHA  $OUT"
