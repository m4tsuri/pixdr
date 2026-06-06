#!/usr/bin/env bash
set -euo pipefail
source "$(cd "$(dirname "$0")" && pwd)/env.sh"

pixdr_require_cmd zip keytool unzip curl jar >/dev/null
BUILD_TOOLS="$(pixdr_android_build_tools)"
AAPT2="${BUILD_TOOLS}/aapt2"
D8="${BUILD_TOOLS}/d8"
ZIPALIGN="${BUILD_TOOLS}/zipalign"
APKSIGNER="${BUILD_TOOLS}/apksigner"
PLATFORM_JAR="${ANDROID_HOME}/platforms/android-${PIXDR_ANDROID_API}/android.jar"

for f in "${AAPT2}" "${D8}" "${ZIPALIGN}" "${APKSIGNER}" "${PLATFORM_JAR}"; do
  [[ -e "$f" ]] || { echo "ERROR: missing Android SDK file: $f" >&2; exit 1; }
done

pixdr_kotlinc() {
  if command -v kotlinc >/dev/null 2>&1; then
    command -v kotlinc
    return 0
  fi

  local version="${PIXDR_KOTLIN_VERSION:-2.2.21}"
  local dir="${PIXDR_BUILD_DIR}/kotlin-compiler-${version}"
  local compiler="${dir}/kotlinc/bin/kotlinc"
  if [[ ! -x "${compiler}" ]]; then
    local zip_path="${PIXDR_BUILD_DIR}/kotlin-compiler-${version}.zip"
    mkdir -p "${PIXDR_BUILD_DIR}"
    echo "  Downloading Kotlin compiler ${version} from GitHub" >&2
    curl -L --fail --retry 3 \
      -o "${zip_path}" \
      "https://github.com/JetBrains/kotlin/releases/download/v${version}/kotlin-compiler-${version}.zip"
    rm -rf "${dir}"
    mkdir -p "${dir}"
    unzip -q "${zip_path}" -d "${dir}"
  fi
  [[ -x "${compiler}" ]] || { echo "ERROR: Kotlin compiler not found: ${compiler}" >&2; exit 1; }
  printf '%s\n' "${compiler}"
}

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

KOTLIN_SOURCES=()
if [[ -d "${PIXDR_APP_DIR}/java" ]]; then
  while IFS= read -r -d '' src; do
    KOTLIN_SOURCES+=("${src}")
  done < <(find "${PIXDR_APP_DIR}/java" -name '*.kt' -print0)
fi

if (( ${#KOTLIN_SOURCES[@]} > 0 )); then
  KOTLINC="$(pixdr_kotlinc)"
  KOTLIN_HOME_DIR="$(cd "$(dirname "${KOTLINC}")/.." && pwd)"
  KOTLIN_STDLIB="${KOTLIN_HOME_DIR}/lib/kotlin-stdlib.jar"
  [[ -f "${KOTLIN_STDLIB}" ]] || { echo "ERROR: missing Kotlin stdlib: ${KOTLIN_STDLIB}" >&2; exit 1; }
  mkdir -p "${TMPDIR}/classes" "${TMPDIR}/dex"
  echo "  Kotlin sources: ${#KOTLIN_SOURCES[@]}"
  "${KOTLINC}" \
    -jvm-target 1.8 \
    -classpath "${PLATFORM_JAR}" \
    -d "${TMPDIR}/classes" \
    "${KOTLIN_SOURCES[@]}"
  jar cf "${TMPDIR}/classes.jar" -C "${TMPDIR}/classes" .
  "${D8}" \
    --min-api "${PIXDR_ANDROID_API}" \
    --lib "${PLATFORM_JAR}" \
    --output "${TMPDIR}/dex" \
    "${TMPDIR}/classes.jar" \
    "${KOTLIN_STDLIB}"
fi

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
  ZIP_INPUTS=(lib/ assets/)
  if [[ -f dex/classes.dex ]]; then
    cp dex/classes.dex ./classes.dex
    ZIP_INPUTS+=(classes.dex)
  fi
  zip -r "${APK_UNSIGNED}" "${ZIP_INPUTS[@]}" >/dev/null
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
