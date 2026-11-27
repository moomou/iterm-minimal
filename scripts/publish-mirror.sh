#!/usr/bin/env bash
set -euo pipefail

# Mirror clean project source to public GitHub repo `moomou/jterm` and
# build + publish macOS Universal & Linux x86_64 release artifacts.
#
# Usage:
#   ./scripts/publish-mirror.sh <tag> [owner/repo]
# Example:
#   ./scripts/publish-mirror.sh v0.1.21

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
TAG="${1:-}"
REPO="${2:-moomou/jterm}"

if [[ -z "$TAG" ]]; then
  echo "Usage: $0 <tag> [owner/repo]" >&2
  exit 1
fi

STAGING="$(mktemp -d /tmp/jterm-mirror.XXXXXX)"
ARTIFACTS="$(mktemp -d /tmp/jterm-artifacts.XXXXXX)"
trap 'rm -rf "$STAGING" "$ARTIFACTS"' EXIT

echo "==> [1/4] Testing & building Linux x86_64 + macOS Universal release artifacts..."
cd "$ROOT"
cargo test --workspace --quiet
cargo build --release --bin jterm
./scripts/build-macos.sh

echo "==> [2/4] Packaging macOS Universal and Linux x86_64 release archives..."
MAC_ZIP="$ARTIFACTS/JTerm-${TAG}-macos-universal.zip"
MAC_TGZ="$ARTIFACTS/JTerm-${TAG}-macos-universal.tar.gz"
LINUX_TGZ="$ARTIFACTS/jterm-${TAG}-linux-x86_64.tar.gz"

python3 - "$ROOT/dist" "$MAC_ZIP" <<'PY'
import os, stat, sys, zipfile
root_dir, out_zip = sys.argv[1], sys.argv[2]
app_dir = os.path.join(root_dir, "JTerm.app")
with zipfile.ZipFile(out_zip, "w", compression=zipfile.ZIP_DEFLATED, compresslevel=9) as zf:
    for dirpath, dirnames, filenames in os.walk(app_dir):
        dirnames.sort()
        filenames.sort()
        for fn in filenames:
            full = os.path.join(dirpath, fn)
            rel = os.path.relpath(full, root_dir)
            st = os.stat(full)
            zi = zipfile.ZipInfo(rel)
            zi.compress_type = zipfile.ZIP_DEFLATED
            zi.create_system = 3  # Unix permissions preserved on macOS unzip
            zi.external_attr = (stat.S_IFREG | (st.st_mode & 0o777)) << 16
            with open(full, "rb") as f:
                zf.writestr(zi, f.read())
PY
tar -C "$ROOT/dist" -czf "$MAC_TGZ" JTerm.app

LINUX_DIR="$ARTIFACTS/jterm-${TAG}-linux-x86_64"
mkdir -p "$LINUX_DIR/shell_integration"
cp "$ROOT/target/release/jterm" "$LINUX_DIR/"
chmod +x "$LINUX_DIR/jterm"
cp "$ROOT/scripts/shell_integration/jterm-shell-integration.sh" "$LINUX_DIR/shell_integration/"
cp "$ROOT/dist/JTerm.app/Contents/Resources/shell_integration/"* "$LINUX_DIR/shell_integration/" 2>/dev/null || true
cp "$ROOT/README.md" "$ROOT/LICENSE" "$LINUX_DIR/"
tar -C "$ARTIFACTS" -czf "$LINUX_TGZ" "jterm-${TAG}-linux-x86_64"

(cd "$ARTIFACTS" && sha256sum \
  "JTerm-${TAG}-macos-universal.zip" \
  "JTerm-${TAG}-macos-universal.tar.gz" \
  "jterm-${TAG}-linux-x86_64.tar.gz" > SHA256SUMS.txt)

echo "==> [3/4] Syncing clean source mirror to $REPO..."
git clone "https://github.com/${REPO}.git" "$STAGING"
rsync -a --delete \
  --exclude='/target' \
  --exclude='/dist' \
  --exclude='/.git' \
  --exclude='*.DS_Store' \
  --exclude='*.log' \
  "$ROOT/" "$STAGING/"

cd "$STAGING"
git add -A
git commit -m "Mirror jterm ${TAG}"
git push origin main
git tag -f "$TAG"
git push --force origin "$TAG"

echo "==> [4/4] Publishing GitHub Release ${TAG} on ${REPO}..."
gh release create "$TAG" \
  "$MAC_ZIP" \
  "$MAC_TGZ" \
  "$LINUX_TGZ" \
  "$ARTIFACTS/SHA256SUMS.txt" \
  --repo "$REPO" \
  --title "jterm ${TAG}" \
  --generate-notes
