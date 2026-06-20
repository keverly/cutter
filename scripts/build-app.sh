#!/usr/bin/env bash
#
# Build a standalone, double-clickable Cutter.app from the cutter-gui binary.
#
#   ./scripts/build-app.sh
#
# Output: dist/Cutter.app
# Install: cp -r dist/Cutter.app /Applications/
#
set -euo pipefail

cd "$(dirname "$0")/.."

APP_NAME="Cutter"
BIN_NAME="cutter-gui"
APP_DIR="dist/${APP_NAME}.app"
CONTENTS="${APP_DIR}/Contents"

# If the full Xcode license hasn't been accepted, the linker's clang invocation
# fails. Fall back to the standalone Command Line Tools toolchain, which has no
# license gate, so the build works without `sudo xcodebuild -license accept`.
if ! /usr/bin/xcrun clang --version >/dev/null 2>&1; then
    if [[ -d /Library/Developer/CommandLineTools ]]; then
        export DEVELOPER_DIR=/Library/Developer/CommandLineTools
        echo "==> Xcode license not accepted; using Command Line Tools toolchain"
    fi
fi

echo "==> Building release binary (${BIN_NAME})"
cargo build --release --features gui --bin "${BIN_NAME}"

echo "==> Assembling ${APP_DIR}"
rm -rf "${APP_DIR}"
mkdir -p "${CONTENTS}/MacOS" "${CONTENTS}/Resources"

cp "target/release/${BIN_NAME}" "${CONTENTS}/MacOS/${BIN_NAME}"
cp "scripts/Info.plist" "${CONTENTS}/Info.plist"

# Optional app icon: drop an AppIcon.icns into scripts/ and it gets embedded.
if [[ -f "scripts/AppIcon.icns" ]]; then
    cp "scripts/AppIcon.icns" "${CONTENTS}/Resources/AppIcon.icns"
fi

# Ad-hoc sign so the bundle is a valid app for TCC (the window-linking feature
# needs Accessibility permission, which is keyed to a signed bundle identity).
# Without a Developer ID the signature changes on every rebuild, so macOS may
# ask you to re-grant Accessibility after rebuilding — grant it once per build.
echo "==> Ad-hoc signing ${APP_DIR}"
codesign --force --deep --sign - "${APP_DIR}" 2>/dev/null \
    || echo "    (codesign unavailable; skipping — Accessibility grants may not stick)"

echo "==> Done: ${APP_DIR}"
echo "    Run:     open ${APP_DIR}"
echo "    Install: cp -r ${APP_DIR} /Applications/"
