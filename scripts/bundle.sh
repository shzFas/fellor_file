#!/usr/bin/env bash
# Build Fellor.app (release) with an ad-hoc signature.
#   scripts/bundle.sh            -> target/release/Fellor.app
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

cargo build --release -p fellor-app

VERSION="$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -n1)"
APP="$ROOT/target/release/Fellor.app"

rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"
cp "$ROOT/target/release/fellor-app" "$APP/Contents/MacOS/Fellor"
sed "s/__VERSION__/${VERSION:-0.1.0}/g" "$ROOT/apps/fellor-app/packaging/Info.plist" > "$APP/Contents/Info.plist"
printf 'APPL????' > "$APP/Contents/PkgInfo"

codesign -s - --force --deep "$APP"
echo "Built $APP"
