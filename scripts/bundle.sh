#!/usr/bin/env bash
# Build Fellor.app and release archives.
#
#   scripts/bundle.sh              -> host-arch build, target/release/Fellor.app
#   scripts/bundle.sh --universal  -> arm64 + x86_64 universal build and
#                                     dist/ with .dmg, .zip, CLI tarball, SHA256SUMS
#
# The app is ad-hoc signed (no Apple Developer ID), so on first launch macOS
# asks the user to confirm — see README "Установка".
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

UNIVERSAL=0
[[ "${1:-}" == "--universal" ]] && UNIVERSAL=1

VERSION="$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -n1)"
VERSION="${VERSION:-0.1.0}"
export MACOSX_DEPLOYMENT_TARGET=13.0

if [[ $UNIVERSAL == 1 ]]; then
  TARGETS=(aarch64-apple-darwin x86_64-apple-darwin)
  for t in "${TARGETS[@]}"; do
    cargo build --release --target "$t" -p fellor-app -p fellor-cli
  done
  OUT="$ROOT/target/universal"
  mkdir -p "$OUT"
  for bin in fellor-app fellor; do
    lipo -create -output "$OUT/$bin" \
      "target/aarch64-apple-darwin/release/$bin" "target/x86_64-apple-darwin/release/$bin"
  done
  BIN_DIR="$OUT"
  APP_DIR="$ROOT/dist"
else
  cargo build --release -p fellor-app -p fellor-cli
  BIN_DIR="$ROOT/target/release"
  APP_DIR="$ROOT/target/release"
fi

APP="$APP_DIR/Fellor.app"
rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"
cp "$BIN_DIR/fellor-app" "$APP/Contents/MacOS/Fellor"
sed "s/__VERSION__/$VERSION/g" "$ROOT/apps/fellor-app/packaging/Info.plist" > "$APP/Contents/Info.plist"
printf 'APPL????' > "$APP/Contents/PkgInfo"
cp "$ROOT/LICENSE" "$APP/Contents/Resources/LICENSE.txt"
codesign -s - --force --deep "$APP"
echo "Built $APP"

if [[ $UNIVERSAL == 1 ]]; then
  cd "$APP_DIR"
  NAME="Fellor-$VERSION-macos-universal"
  rm -f "$NAME.zip" "$NAME.dmg" "fellor-cli-$VERSION-macos-universal.tar.gz" SHA256SUMS.txt

  ditto -c -k --keepParent Fellor.app "$NAME.zip"

  STAGE="$(mktemp -d)"
  cp -R Fellor.app "$STAGE/"
  ln -s /Applications "$STAGE/Applications"
  cp "$ROOT/LICENSE" "$STAGE/LICENSE.txt"
  hdiutil create -volname "Fellor File" -srcfolder "$STAGE" -ov -format UDZO "$NAME.dmg" >/dev/null
  rm -rf "$STAGE"

  CLI_STAGE="$(mktemp -d)"
  cp "$BIN_DIR/fellor" "$CLI_STAGE/fellor"
  codesign -s - --force "$CLI_STAGE/fellor"
  cp "$ROOT/LICENSE" "$CLI_STAGE/LICENSE.txt"
  tar -czf "fellor-cli-$VERSION-macos-universal.tar.gz" -C "$CLI_STAGE" fellor LICENSE.txt
  rm -rf "$CLI_STAGE"

  shasum -a 256 "$NAME.dmg" "$NAME.zip" "fellor-cli-$VERSION-macos-universal.tar.gz" > SHA256SUMS.txt
  echo "Release files in $APP_DIR:"
  ls -lh "$NAME.dmg" "$NAME.zip" "fellor-cli-$VERSION-macos-universal.tar.gz" SHA256SUMS.txt
fi
