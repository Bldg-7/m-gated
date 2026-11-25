#!/usr/bin/env bash
# DeepSeek 어댑터를 WASM 컴포넌트로 빌드한다(결정 0009).
# 결과: target/adapters/deepseek.wasm. 마지막 줄에 SHA-256을 쓴다.
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
  sha256sum "$OUT"
else
  shasum -a 256 "$OUT"
fi
