#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
VERSION="${1:-$(/usr/bin/awk -F '"' '/^version = / { print $2; exit }' "$ROOT/Cargo.toml")}"
BUILD_NUMBER="${PANERU_BUILD_NUMBER:-${GITHUB_RUN_NUMBER:-1}}"
BUILD_ARCHS="${PANERU_BUILD_ARCHS:-host}"
BUILD_ROOT="$ROOT/.build/release"
APP_DIR="$BUILD_ROOT/Paneru.app"
CONTENTS_DIR="$APP_DIR/Contents"
MACOS_DIR="$CONTENTS_DIR/MacOS"
FRAMEWORKS_DIR="$CONTENTS_DIR/Frameworks"
EXECUTABLE="$MACOS_DIR/paneru"
SIGN_IDENTITY="${PANERU_SIGN_IDENTITY:--}"
CARGO_BIN="${CARGO:-}"

if [[ -z "$CARGO_BIN" ]]; then
  if command -v cargo >/dev/null 2>&1; then
    CARGO_BIN="$(command -v cargo)"
  elif command -v rustup >/dev/null 2>&1; then
    CARGO_BIN="$(rustup which cargo)"
  else
    echo "cargo was not found. Install Rust with rustup before building Paneru." >&2
    exit 1
  fi
fi
if ! command -v rustc >/dev/null 2>&1 && command -v rustup >/dev/null 2>&1; then
  RUSTC_BIN="$(rustup which rustc)"
  export PATH="$(dirname "$RUSTC_BIN"):$PATH"
fi

if [[ -z "$VERSION" ]]; then
  echo "Unable to resolve the Paneru version." >&2
  exit 1
fi

case "$BUILD_ARCHS" in
  universal)
    "$CARGO_BIN" build --locked --release --target aarch64-apple-darwin
    "$CARGO_BIN" build --locked --release --target x86_64-apple-darwin
    /bin/mkdir -p "$BUILD_ROOT"
    /usr/bin/lipo -create \
      "$ROOT/target/aarch64-apple-darwin/release/paneru" \
      "$ROOT/target/x86_64-apple-darwin/release/paneru" \
      -output "$BUILD_ROOT/paneru"
    BUILT_EXECUTABLE="$BUILD_ROOT/paneru"
    ;;
  host)
    "$CARGO_BIN" build --locked --release
    BUILT_EXECUTABLE="$ROOT/target/release/paneru"
    ;;
  arm64|aarch64)
    "$CARGO_BIN" build --locked --release --target aarch64-apple-darwin
    BUILT_EXECUTABLE="$ROOT/target/aarch64-apple-darwin/release/paneru"
    ;;
  x86_64)
    "$CARGO_BIN" build --locked --release --target x86_64-apple-darwin
    BUILT_EXECUTABLE="$ROOT/target/x86_64-apple-darwin/release/paneru"
    ;;
  *)
    echo "PANERU_BUILD_ARCHS must be host, universal, arm64, or x86_64." >&2
    exit 1
    ;;
esac

SPARKLE_ROOT="${SPARKLE_DIR:-$("$ROOT/scripts/download-sparkle.sh")}"
if [[ -d "$SPARKLE_ROOT/Sparkle.framework" ]]; then
  SPARKLE_FRAMEWORK="$SPARKLE_ROOT/Sparkle.framework"
else
  SPARKLE_FRAMEWORK="$(/usr/bin/find "$SPARKLE_ROOT" -type d -name Sparkle.framework \
    -path '*macos-arm64_x86_64*' -print -quit)"
fi
if [[ -z "$SPARKLE_FRAMEWORK" ]]; then
  echo "Universal Sparkle.framework was not found under $SPARKLE_ROOT." >&2
  exit 1
fi

/bin/rm -rf "$APP_DIR"
/bin/mkdir -p "$MACOS_DIR" "$FRAMEWORKS_DIR"
/bin/cp "$BUILT_EXECUTABLE" "$EXECUTABLE"
/bin/chmod 755 "$EXECUTABLE"
/bin/cp "$ROOT/assets/Info.plist" "$CONTENTS_DIR/Info.plist"
/usr/bin/ditto "$SPARKLE_FRAMEWORK" "$FRAMEWORKS_DIR/Sparkle.framework"

/usr/bin/plutil -replace CFBundleShortVersionString -string "$VERSION" "$CONTENTS_DIR/Info.plist"
/usr/bin/plutil -replace CFBundleVersion -string "$BUILD_NUMBER" "$CONTENTS_DIR/Info.plist"

if [[ "$BUILD_ARCHS" == universal ]]; then
  EXECUTABLE_ARCHS="$(/usr/bin/lipo -archs "$EXECUTABLE")"
  [[ "$EXECUTABLE_ARCHS" == *arm64* && "$EXECUTABLE_ARCHS" == *x86_64* ]] || {
    echo "The packaged executable is not universal: $EXECUTABLE_ARCHS" >&2
    exit 1
  }
fi

sign_path() {
  local path="$1"
  if [[ "$SIGN_IDENTITY" == "-" ]]; then
    /usr/bin/codesign --force --sign - "$path"
  else
    /usr/bin/codesign --force --options runtime --timestamp --sign "$SIGN_IDENTITY" "$path"
  fi
}

SPARKLE_VERSION_DIR="$FRAMEWORKS_DIR/Sparkle.framework/Versions/B"
for nested in \
  "$SPARKLE_VERSION_DIR/XPCServices/Downloader.xpc" \
  "$SPARKLE_VERSION_DIR/XPCServices/Installer.xpc" \
  "$SPARKLE_VERSION_DIR/Updater.app" \
  "$SPARKLE_VERSION_DIR/Autoupdate"; do
  [[ ! -e "$nested" ]] || sign_path "$nested"
done
sign_path "$FRAMEWORKS_DIR/Sparkle.framework"
sign_path "$APP_DIR"

/usr/bin/codesign --verify --deep --strict --verbose=2 "$APP_DIR"
printf '%s\n' "$APP_DIR"
