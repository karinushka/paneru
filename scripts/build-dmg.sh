#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
VERSION="${1:-$(/usr/bin/awk -F '"' '/^version = / { print $2; exit }' "$ROOT/Cargo.toml")}"
APP_DIR="${PANERU_APP_PATH:-$ROOT/.build/release/Paneru.app}"
STAGE_DIR="$ROOT/.build/dmg-root"
DIST_DIR="$ROOT/dist"
DMG_PATH="$DIST_DIR/Paneru-$VERSION.dmg"
SIGN_IDENTITY="${PANERU_SIGN_IDENTITY:--}"

if [[ ! -d "$APP_DIR" ]]; then
  echo "Paneru.app was not found at $APP_DIR. Run scripts/build-app.sh first." >&2
  exit 1
fi

/bin/rm -rf "$STAGE_DIR" "$DMG_PATH"
/bin/mkdir -p "$STAGE_DIR" "$DIST_DIR"
/usr/bin/ditto "$APP_DIR" "$STAGE_DIR/Paneru.app"
/bin/ln -s /Applications "$STAGE_DIR/Applications"

/usr/bin/hdiutil create \
  -volname Paneru \
  -srcfolder "$STAGE_DIR" \
  -ov \
  -format UDZO \
  -imagekey zlib-level=9 \
  "$DMG_PATH"

if [[ "$SIGN_IDENTITY" != "-" ]]; then
  /usr/bin/codesign --force --timestamp --sign "$SIGN_IDENTITY" "$DMG_PATH"
fi
/usr/bin/hdiutil verify "$DMG_PATH"
/bin/rm -rf "$STAGE_DIR"
printf '%s\n' "$DMG_PATH"
