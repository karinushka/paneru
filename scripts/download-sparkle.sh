#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
SPARKLE_VERSION="${SPARKLE_VERSION:-2.9.4}"
SPARKLE_SHA256="${SPARKLE_SHA256:-ce89daf967db1e1893ed3ebd67575ed82d3902563e3191ca92aaec9164fbdef9}"
CACHE_ROOT="${PANERU_DEPENDENCIES_DIR:-$ROOT/.build/dependencies}"
DESTINATION="$CACHE_ROOT/sparkle-$SPARKLE_VERSION"
ARCHIVE="$CACHE_ROOT/Sparkle-$SPARKLE_VERSION.tar.xz"

framework_path() {
  if [[ -d "$DESTINATION/Sparkle.framework" ]]; then
    printf '%s\n' "$DESTINATION/Sparkle.framework"
  else
    /usr/bin/find "$DESTINATION" -type d -name Sparkle.framework \
      -path '*macos-arm64_x86_64*' -print -quit 2>/dev/null
  fi
}

if [[ -n "$(framework_path)" ]] && [[ -x "$DESTINATION/bin/generate_appcast" ]]; then
  printf '%s\n' "$DESTINATION"
  exit 0
fi

/bin/mkdir -p "$CACHE_ROOT"
/usr/bin/curl --fail --location --silent --show-error \
  "https://github.com/sparkle-project/Sparkle/releases/download/$SPARKLE_VERSION/Sparkle-$SPARKLE_VERSION.tar.xz" \
  --output "$ARCHIVE"

printf '%s  %s\n' "$SPARKLE_SHA256" "$ARCHIVE" | /usr/bin/shasum --algorithm 256 --check
/bin/rm -rf "$DESTINATION"
/bin/mkdir -p "$DESTINATION"
/usr/bin/tar -xJf "$ARCHIVE" -C "$DESTINATION"

if [[ -z "$(framework_path)" ]]; then
  echo "Sparkle.framework was not found in the downloaded archive." >&2
  exit 1
fi
if [[ ! -x "$DESTINATION/bin/generate_appcast" ]]; then
  echo "Sparkle signing tools were not found in the downloaded archive." >&2
  exit 1
fi

printf '%s\n' "$DESTINATION"
