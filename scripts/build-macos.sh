#!/usr/bin/env bash
# Build a release Universal (Apple Silicon arm64 + Intel x86_64) macOS JTerm.app bundle on Linux or macOS.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
HOST_OS="$(uname -s)"

if [[ "${HOST_OS}" != "Darwin" ]]; then
  SDK="${MACOS_SDK_PATH:-${HOME}/.cache/macos-sdk/MacOSX26.1.sdk}"
  export SDKROOT="${SDK}"
  if [[ ! -d "${SDK}" ]]; then
    echo "macOS SDK missing at ${SDK}; running scripts/setup-macos-sdk.sh..."
    "${ROOT}/scripts/setup-macos-sdk.sh"
  fi
fi
export MACOSX_DEPLOYMENT_TARGET="${MACOSX_DEPLOYMENT_TARGET:-13.0}"

cd "${ROOT}"

echo "==> [1/5] Building aarch64-apple-darwin (Apple Silicon, release LTO)..."
cargo build --release --target aarch64-apple-darwin --bin jterm

echo "==> [2/5] Building x86_64-apple-darwin (Intel, release LTO)..."
cargo build --release --target x86_64-apple-darwin --bin jterm

APP_DIR="${ROOT}/dist/JTerm.app"
rm -rf "${APP_DIR}"
mkdir -p "${APP_DIR}/Contents/MacOS" "${APP_DIR}/Contents/Resources/shell_integration"

echo "==> [3/5] Creating Universal (arm64 + x86_64) Mach-O binary & .app bundle..."
python3 "${ROOT}/scripts/make-universal.py" \
  "${APP_DIR}/Contents/MacOS/jterm" \
  "${ROOT}/target/aarch64-apple-darwin/release/jterm" \
  "${ROOT}/target/x86_64-apple-darwin/release/jterm"
chmod +x "${APP_DIR}/Contents/MacOS/jterm"

printf 'APPL????' > "${APP_DIR}/Contents/PkgInfo"

cat > "${APP_DIR}/Contents/Info.plist" << 'PLIST'
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleDevelopmentRegion</key>
  <string>en</string>
  <key>CFBundleDisplayName</key>
  <string>JTerm</string>
  <key>CFBundleExecutable</key>
  <string>jterm</string>
  <key>CFBundleIconFile</key>
  <string>AppIcon</string>
  <key>CFBundleIconName</key>
  <string>AppIcon</string>
  <key>CFBundleIdentifier</key>
  <string>dev.moomou.jterm</string>
  <key>CFBundleInfoDictionaryVersion</key>
  <string>6.0</string>
  <key>CFBundleName</key>
  <string>JTerm</string>
  <key>CFBundlePackageType</key>
  <string>APPL</string>
  <key>CFBundleShortVersionString</key>
  <string>0.1.25</string>
  <key>CFBundleSupportedPlatforms</key>
  <array>
    <string>MacOSX</string>
  </array>
  <key>CFBundleVersion</key>
  <string>25</string>
  <key>LSApplicationCategoryType</key>
  <string>public.app-category.developer-tools</string>
  <key>LSMinimumSystemVersion</key>
  <string>13.0</string>
  <key>NSHighResolutionCapable</key>
  <true/>
  <key>NSRequiresAquaSystemAppearance</key>
  <false/>
  <key>NSSupportsAutomaticGraphicsSwitching</key>
  <true/>
  <key>NSPrincipalClass</key>
  <string>NSApplication</string>
  <key>NSAppleEventsUsageDescription</key>
  <string>A program running in JTerm would like to use AppleScript and Automation.</string>
  <key>NSBluetoothAlwaysUsageDescription</key>
  <string>A program running in JTerm would like to use Bluetooth.</string>
  <key>NSCalendarsUsageDescription</key>
  <string>A program running in JTerm would like to access your calendar.</string>
  <key>NSCalendarsFullAccessUsageDescription</key>
  <string>A program running in JTerm would like to access your calendar.</string>
  <key>NSCameraUsageDescription</key>
  <string>A program running in JTerm would like to use the camera.</string>
  <key>NSContactsUsageDescription</key>
  <string>A program running in JTerm would like to access your contacts.</string>
  <key>NSDesktopFolderUsageDescription</key>
  <string>A program running in JTerm would like to access files in your Desktop folder.</string>
  <key>NSDocumentsFolderUsageDescription</key>
  <string>A program running in JTerm would like to access files in your Documents folder.</string>
  <key>NSDownloadsFolderUsageDescription</key>
  <string>A program running in JTerm would like to access files in your Downloads folder.</string>
  <key>NSNetworkVolumesUsageDescription</key>
  <string>A program running in JTerm would like to access files on a network volume.</string>
  <key>NSRemovableVolumesUsageDescription</key>
  <string>A program running in JTerm would like to access files on a removable volume.</string>
  <key>NSLocalNetworkUsageDescription</key>
  <string>A program running in JTerm would like to find and communicate with devices on your local network.</string>
  <key>NSBonjourServices</key>
  <array>
    <string>_ssh._tcp.</string>
    <string>_sftp-ssh._tcp.</string>
    <string>_http._tcp.</string>
  </array>
  <key>NSLocationAlwaysUsageDescription</key>
  <string>A program running in JTerm would like to use your location information.</string>
  <key>NSLocationUsageDescription</key>
  <string>A program running in JTerm would like to use your location information.</string>
  <key>NSLocationWhenInUseUsageDescription</key>
  <string>A program running in JTerm would like to use your location information.</string>
  <key>NSMicrophoneUsageDescription</key>
  <string>A program running in JTerm would like to use the microphone.</string>
  <key>NSPhotoLibraryUsageDescription</key>
  <string>A program running in JTerm would like to access your photo library.</string>
  <key>NSRemindersUsageDescription</key>
  <string>A program running in JTerm would like to access your reminders.</string>
  <key>NSRemindersFullAccessUsageDescription</key>
  <string>A program running in JTerm would like to access your reminders.</string>
  <key>NSSpeechRecognitionUsageDescription</key>
  <string>A program running in JTerm would like to use speech recognition.</string>
  <key>NSSystemAdministrationUsageDescription</key>
  <string>A program running in JTerm requires elevated system permissions.</string>
</dict>
</plist>
PLIST

cp -a "${ROOT}/assets/AppIcon.icns" "${APP_DIR}/Contents/Resources/AppIcon.icns"
cp -a "${ROOT}/scripts/shell_integration/"* "${APP_DIR}/Contents/Resources/shell_integration/"
# Also bundle user's standard imgcat & it2dl utilities if present on build host
for util in imgcat it2dl; do
  if [[ -f "${HOME}/.iterm2/${util}" && ! -f "${APP_DIR}/Contents/Resources/shell_integration/${util}" ]]; then
    cp -a "${HOME}/.iterm2/${util}" "${APP_DIR}/Contents/Resources/shell_integration/${util}"
  fi
done

echo "==> [4/5] Sealing & code-signing JTerm.app bundle (dev.moomou.jterm + Info.plist + CodeResources)..."
if [[ "${HOST_OS}" == "Darwin" ]]; then
  SIGN_ID="${JTERM_CODESIGN_IDENTITY:--}"
  /usr/bin/codesign --force --deep --sign "${SIGN_ID}" --identifier dev.moomou.jterm "${APP_DIR}"
  /usr/bin/codesign --verify --deep --strict --verbose=4 "${APP_DIR}"
else
  RCODESIGN="${RCODESIGN_BIN:-${HOME}/.cache/macos-tools/rcodesign}"
  if [[ ! -x "${RCODESIGN}" ]]; then
    "${ROOT}/scripts/setup-rcodesign.sh"
  fi
  SIGN_ARGS=(--binary-identifier dev.moomou.jterm)
  if [[ -n "${JTERM_SIGN_P12:-}" ]]; then
    SIGN_ARGS+=(--p12-file "${JTERM_SIGN_P12}")
    if [[ -n "${JTERM_SIGN_P12_PASSWORD:-}" ]]; then
      SIGN_ARGS+=(--p12-password "${JTERM_SIGN_P12_PASSWORD}")
    fi
  elif [[ -n "${JTERM_SIGN_PEM:-}" ]]; then
    SIGN_ARGS+=(--pem-file "${JTERM_SIGN_PEM}")
  fi
  "${RCODESIGN}" sign "${SIGN_ARGS[@]}" "${APP_DIR}"
fi

echo "==> [5/5] Verifying Universal Mach-O headers, frameworks, and sealed bundle code signature..."
python3 "${ROOT}/scripts/verify-macho.py" --bundle "${APP_DIR}" --expect-ident dev.moomou.jterm "${APP_DIR}/Contents/MacOS/jterm"

echo ""
echo "DONE: macOS Universal app bundle ready at:"
echo "  ${APP_DIR}"
echo ""
echo "Deploy to Mac:"
echo "  rsync -av ${APP_DIR} <mac>:/Applications/"
echo "  # First launch on macOS: xattr -dr com.apple.quarantine /Applications/JTerm.app"
echo "  # Optional (persistent macOS TCC permissions across upgrades without an Apple Dev account):"
echo "  #   ./scripts/sign-local-macos.sh /Applications/JTerm.app"
