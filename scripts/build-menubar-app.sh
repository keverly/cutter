#!/usr/bin/env bash
#
# Build the standalone menu bar app, Cutter Menu.app, from the cutter-menubar
# binary. It lives only in the menu bar (no window, no Dock icon) and runs
# independently of Cutter.app.
#
#   ./scripts/build-menubar-app.sh
#
# Output: dist/Cutter Menu.app
# Install: cp -r "dist/Cutter Menu.app" /Applications/
#
set -euo pipefail

cd "$(dirname "$0")/.."

APP_NAME="Cutter Menu"
BIN_NAME="cutter-menubar"
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
cargo build --release --features menubar --bin "${BIN_NAME}"

echo "==> Assembling ${APP_DIR}"
rm -rf "${APP_DIR}"
mkdir -p "${CONTENTS}/MacOS" "${CONTENTS}/Resources"

cp "target/release/${BIN_NAME}" "${CONTENTS}/MacOS/${BIN_NAME}"
cp "scripts/MenuBar-Info.plist" "${CONTENTS}/Info.plist"

# Shares Cutter.app's optional icon (scripts/AppIcon.icns), shown in Finder.
if [[ -f "scripts/AppIcon.icns" ]]; then
    cp "scripts/AppIcon.icns" "${CONTENTS}/Resources/AppIcon.icns"
fi

echo "==> Ad-hoc signing ${APP_DIR}"
codesign --force --deep --sign - "${APP_DIR}" 2>/dev/null \
    || echo "    (codesign unavailable; skipping)"

echo "==> Done: ${APP_DIR}"
echo "    Run:     open \"${APP_DIR}\""
echo "    Install: cp -r \"${APP_DIR}\" /Applications/"
