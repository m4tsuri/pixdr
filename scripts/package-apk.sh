#!/usr/bin/env bash
set -euo pipefail
source "$(cd "$(dirname "$0")" && pwd)/env.sh"

pixdr_require_cmd zip keytool >/dev/null
BUILD_TOOLS="$(pixdr_android_build_tools)"
AAPT2="${BUILD_TOOLS}/aapt2"
ZIPALIGN="${BUILD_TOOLS}/zipalign"
APKSIGNER="${BUILD_TOOLS}/apksigner"
PLATFORM_JAR="${ANDROID_HOME}/platforms/android-${PIXDR_ANDROID_API}/android.jar"

for f in "${AAPT2}" "${ZIPALIGN}" "${APKSIGNER}" "${PLATFORM_JAR}"; do
  [[ -e "$f" ]] || { echo "ERROR: missing Android SDK file: $f" >&2; exit 1; }
done

LIBPIXDR="${PIXDR_APP_DIR}/target/${PIXDR_RUST_TARGET}/release/libpixdr.so"
LIBUHD="${PIXDR_PREFIX}/lib/libuhd.so"
LIBCXX="${NDK_SYSROOT}/usr/lib/${PIXDR_RUST_TARGET}/libc++_shared.so"
for f in "${LIBPIXDR}" "${LIBUHD}" "${LIBCXX}"; do
  [[ -f "$f" ]] || { echo "ERROR: missing native library: $f" >&2; exit 1; }
done

TMPDIR="${PIXDR_APP_DIR}/target/apk-tmp"
APK_UNSIGNED="${PIXDR_APP_DIR}/target/pixdr-unsigned.apk"
APK_ALIGNED="${PIXDR_APP_DIR}/target/pixdr-aligned.apk"
APK_FINAL="${PIXDR_APK}"

rm -rf "${TMPDIR}"
mkdir -p "${TMPDIR}/lib/${PIXDR_ANDROID_ABI}" "${PIXDR_APP_DIR}/target"

echo "=== Packaging pixdr APK ==="
echo "SDK: ${ANDROID_HOME}"
echo "Build-tools: ${BUILD_TOOLS}"
echo "Platform: ${PLATFORM_JAR}"

"${AAPT2}" compile \
  --dir "${PIXDR_APP_DIR}/res" \
  -o "${TMPDIR}/compiled-resources.zip" \
  --no-crunch

"${AAPT2}" link \
  -o "${APK_UNSIGNED}" \
  -I "${PLATFORM_JAR}" \
  --manifest "${PIXDR_APP_DIR}/AndroidManifest.xml" \
  --auto-add-overlay \
  "${TMPDIR}/compiled-resources.zip"

cp "${LIBPIXDR}" "${TMPDIR}/lib/${PIXDR_ANDROID_ABI}/"
cp "${LIBUHD}" "${TMPDIR}/lib/${PIXDR_ANDROID_ABI}/"
cp "${LIBCXX}" "${TMPDIR}/lib/${PIXDR_ANDROID_ABI}/"

echo "  libpixdr.so: $(ls -lh "${TMPDIR}/lib/${PIXDR_ANDROID_ABI}/libpixdr.so" | awk '{print $5}')"
echo "  libuhd.so:   $(ls -lh "${TMPDIR}/lib/${PIXDR_ANDROID_ABI}/libuhd.so" | awk '{print $5}')"
echo "  libc++_shared.so: $(ls -lh "${TMPDIR}/lib/${PIXDR_ANDROID_ABI}/libc++_shared.so" | awk '{print $5}')"

if [[ -d "${PIXDR_APP_DIR}/assets" ]] && ls "${PIXDR_APP_DIR}"/assets/usrp_b200* >/dev/null 2>&1; then
  mkdir -p "${TMPDIR}/assets"
  cp "${PIXDR_APP_DIR}"/assets/usrp_b200_fw.hex "${TMPDIR}/assets/" 2>/dev/null || true
  cp "${PIXDR_APP_DIR}"/assets/usrp_b200_fpga.bin "${TMPDIR}/assets/" 2>/dev/null || true
  cp "${PIXDR_APP_DIR}"/assets/usrp_b210_fpga.bin "${TMPDIR}/assets/" 2>/dev/null || true
  echo "  Firmware/FPGA assets added"
else
  echo "  WARNING: no UHD firmware/FPGA images found in app/assets/"
fi

(
  cd "${TMPDIR}"
  zip -r "${APK_UNSIGNED}" lib/ assets/ >/dev/null
)

"${ZIPALIGN}" -f -p 4 "${APK_UNSIGNED}" "${APK_ALIGNED}"

DEBUG_KEYSTORE="${HOME}/.android/debug.keystore"
if [[ ! -f "${DEBUG_KEYSTORE}" ]]; then
  mkdir -p "$(dirname "${DEBUG_KEYSTORE}")"
  keytool -genkey -v \
    -keystore "${DEBUG_KEYSTORE}" \
    -storepass android -alias androiddebugkey -keypass android \
    -keyalg RSA -keysize 2048 -validity 10000 \
    -dname "CN=Android Debug,O=Android,C=US" >/dev/null 2>&1
fi

"${APKSIGNER}" sign \
  --ks "${DEBUG_KEYSTORE}" \
  --ks-pass pass:android \
  --ks-key-alias androiddebugkey \
  --key-pass pass:android \
  --min-sdk-version "${PIXDR_ANDROID_API}" \
  --out "${APK_FINAL}" \
  "${APK_ALIGNED}"

cat <<EOF

=========================================
  APK built: ${APK_FINAL}
  Size: $(ls -lh "${APK_FINAL}" | awk '{print $5}')

  Install: scripts/install-run.sh
  Logs:    scripts/logcat.sh
=========================================
EOF
