#!/usr/bin/env bash
set -euo pipefail
source "$(cd "$(dirname "$0")" && pwd)/env.sh"

pixdr_require_cmd cargo cargo-ndk >/dev/null

if [[ ! -f "${PIXDR_PREFIX}/lib/libuhd.so" ]]; then
  echo "ERROR: libuhd.so not found: ${PIXDR_PREFIX}/lib/libuhd.so" >&2
  echo "Run scripts/build-native.sh first, or point PIXDR_PREFIX at an existing UHD Android prefix." >&2
  exit 1
fi

pixdr_print_env

echo "=== Building Rust NativeActivity library ==="
cd "${PIXDR_APP_DIR}"
cargo ndk \
  --target "${PIXDR_RUST_TARGET}" \
  --platform "${PIXDR_ANDROID_API}" \
  -- build --release

LIB="${PIXDR_APP_DIR}/target/${PIXDR_RUST_TARGET}/release/libpixdr.so"
echo "Built: ${LIB} ($(ls -lh "${LIB}" | awk '{print $5}'))"
