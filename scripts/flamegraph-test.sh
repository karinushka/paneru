#!/usr/bin/env bash
set -euo pipefail
unset DEVELOPER_DIR
export PATH="$(echo $PATH | tr ':' '\n' | grep -v 'xcbuild.*xcrun' | paste -sd: -):$PATH"

# STATIC CONFIG — edit these to match your plist
PLIST="$HOME/Library/LaunchAgents/com.github.karinushka.paneru.plist"

# STATIC

# Extract the last ProgramArguments element, i.e. the -c command string
CMD_STR="$(
	/usr/libexec/PlistBuddy -c 'Print :ProgramArguments:2' "$PLIST" 2>/dev/null |
		sed -E 's/^[[:space:]]*//; s/[[:space:]]*$//'
)"

if [[ -z "$CMD_STR" ]]; then
	echo "Failed to extract :ProgramArguments:2" >&2
	exit 1
fi

# Extract path after the last "exec "
EXEC_PATH="$(
	echo "$CMD_STR" |
		sed -E 's/.*exec[[:space:]]+([^[:space:]]+).*/\1/' |
		tr -d '\r'
)"

if [[ -z "$EXEC_PATH" || "$EXEC_PATH" != /* ]]; then
	echo "Could not extract exec-path from ProgramArguments[2]" >&2
	exit 1
fi
if [[ ! -r "$EXEC_PATH" ]]; then
	echo "Exec-path not readable: $EXEC_PATH" >&2
	exit 1
fi

# Validate it looks like bash script (your wrapper uses bash)
head1="$(head -n 1 "$EXEC_PATH" | tr -d '\r')"
if ! echo "$head1" | grep -qE '^#! */.*bash'; then
	echo "Refusing to source: first line isn't a bash shebang." >&2
	echo "First line: $head1" >&2
	exit 1
fi

tmp="$(mktemp)"
trap 'rm -f "$tmp"' EXIT

# Source everything except a trailing exec line (optional)
if tail -n 1 "$EXEC_PATH" | grep -qE '^exec[[:space:]]'; then
	head -n -1 "$EXEC_PATH" >"$tmp"
else
	cp "$EXEC_PATH" "$tmp"
fi

# shellcheck source=/dev/null
source "$tmp"

echo "Sourced env from $EXEC_PATH (minus trailing exec line if present)"echo "Done: sourced env from final-arg file (minus trailing exec if present)."
cargo flamegraph -f lua,luajit --profile fast-release
