#!/usr/bin/env bash
set -euo pipefail
source "$(cd "$(dirname "$0")" && pwd)/env.sh"

pixdr_require_cmd adb >/dev/null
[[ -f "${PIXDR_APK}" ]] || { echo "ERROR: APK not found: ${PIXDR_APK}. Run scripts/build-apk.sh first." >&2; exit 1; }

echo "=== Installing ${PIXDR_APK} ==="
adb install -r "${PIXDR_APK}"

echo "=== Starting ${PIXDR_PACKAGE}/${PIXDR_ACTIVITY} ==="
adb shell am force-stop "${PIXDR_PACKAGE}" || true
adb logcat -c || true
adb shell am start -n "${PIXDR_PACKAGE}/${PIXDR_ACTIVITY}"

echo "\nFollow logs with: scripts/logcat.sh"
