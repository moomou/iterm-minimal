#!/usr/bin/env bash
# Build a release Universal (Apple Silicon arm64 + Intel x86_64) macOS iTermMinimal.app bundle on Linux.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
SDK="${MACOS_SDK_PATH:-${HOME}/.cache/macos-sdk/MacOSX26.1.sdk}"
export SDKROOT="${SDK}"
export MACOSX_DEPLOYMENT_TARGET="${MACOSX_DEPLOYMENT_TARGET:-13.0}"

if [[ ! -d "${SDK}" ]]; then
  echo "macOS SDK missing at ${SDK}; running scripts/setup-macos-sdk.sh..."
  "${ROOT}/scripts/setup-macos-sdk.sh"
fi

cd "${ROOT}"

echo "==> [1/4] Building aarch64-apple-darwin (Apple Silicon, release LTO)..."
cargo build --release --target aarch64-apple-darwin --bin iterm-minimal

echo "==> [2/4] Building x86_64-apple-darwin (Intel, release LTO)..."
cargo build --release --target x86_64-apple-darwin --bin iterm-minimal

APP_DIR="${ROOT}/dist/iTermMinimal.app"
rm -rf "${APP_DIR}"
mkdir -p "${APP_DIR}/Contents/MacOS" "${APP_DIR}/Contents/Resources/shell_integration"

echo "==> [3/4] Creating Universal (arm64 + x86_64) Mach-O binary & .app bundle..."
python3 "${ROOT}/scripts/make-universal.py" \
  "${APP_DIR}/Contents/MacOS/iterm-minimal" \
  "${ROOT}/target/aarch64-apple-darwin/release/iterm-minimal" \
  "${ROOT}/target/x86_64-apple-darwin/release/iterm-minimal"
chmod +x "${APP_DIR}/Contents/MacOS/iterm-minimal"

printf 'APPL????' > "${APP_DIR}/Contents/PkgInfo"

cat > "${APP_DIR}/Contents/Info.plist" << 'PLIST'
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleDevelopmentRegion</key>
  <string>en</string>
  <key>CFBundleDisplayName</key>
  <string>iTermMinimal</string>
  <key>CFBundleExecutable</key>
  <string>iterm-minimal</string>
  <key>CFBundleIdentifier</key>
  <string>dev.moomou.iterm-minimal</string>
  <key>CFBundleInfoDictionaryVersion</key>
  <string>6.0</string>
  <key>CFBundleName</key>
  <string>iTermMinimal</string>
  <key>CFBundlePackageType</key>
  <string>APPL</string>
  <key>CFBundleShortVersionString</key>
  <string>0.1.16</string>
  <key>CFBundleVersion</key>
  <string>16</string>
  <key>LSApplicationCategoryType</key>
  <string>public.app-category.developer-tools</string>
  <key>LSMinimumSystemVersion</key>
  <string>13.0</string>
  <key>NSHighResolutionCapable</key>
  <true/>
  <key>NSSupportsAutomaticGraphicsSwitching</key>
  <true/>
  <key>NSPrincipalClass</key>
  <string>NSApplication</string>
</dict>
</plist>
PLIST

cp -a "${ROOT}/scripts/shell_integration/"* "${APP_DIR}/Contents/Resources/shell_integration/"
# Also bundle user's standard imgcat & it2dl utilities if present on build host
for util in imgcat it2dl; do
  if [[ -f "${HOME}/.iterm2/${util}" && ! -f "${APP_DIR}/Contents/Resources/shell_integration/${util}" ]]; then
    cp -a "${HOME}/.iterm2/${util}" "${APP_DIR}/Contents/Resources/shell_integration/${util}"
  fi
done

echo "==> [4/4] Verifying Universal Mach-O headers, frameworks, and ad-hoc code signatures..."
python3 "${ROOT}/scripts/verify-macho.py" "${APP_DIR}/Contents/MacOS/iterm-minimal"

echo ""
echo "DONE: macOS Universal app bundle ready at:"
echo "  ${APP_DIR}"
echo ""
echo "Deploy to Mac:"
echo "  rsync -av ${APP_DIR} <mac>:/Applications/"
echo "  # First launch on macOS: xattr -dr com.apple.quarantine /Applications/iTermMinimal.app"
