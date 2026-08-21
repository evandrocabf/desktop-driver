#!/usr/bin/env bash

# Exercises the path an installation without Git uses: refresh a tarball
# checkout in place, replace an installer-owned old binary atomically, update
# the skill source, and leave persistent browser credentials untouched.

set -eu

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BINARY="$ROOT/target/release/desktop"
EXPECTED_VERSION="$(sed -n 's/^version = "\([0-9][0-9.]*\)"$/\1/p' "$ROOT/Cargo.toml" | head -n 1)"

[ -x "$BINARY" ] || {
  printf 'error: build the release binary before running this smoke test\n' >&2
  exit 1
}

SCRATCH="$(mktemp -d /tmp/desktop-driver-installer-update.XXXXXX)"
trap 'rm -rf "$SCRATCH"' EXIT

OLD="$SCRATCH/checkout"
ARCHIVE_ROOT="$SCRATCH/archive/desktop-driver-$EXPECTED_VERSION"
PREFIX="$SCRATCH/bin"
STATE="$SCRATCH/state"
DATA="$SCRATCH/data"
FAKE_BIN="$SCRATCH/fake-bin"

mkdir -p "$OLD/crates/desktop-cli" "$OLD/skills/desktop-driver" "$OLD/target/release"
mkdir -p "$ARCHIVE_ROOT/crates/desktop-cli" "$ARCHIVE_ROOT/skills/desktop-driver"
mkdir -p "$ARCHIVE_ROOT/target/release" "$PREFIX" "$STATE/desktop-driver" "$FAKE_BIN"

cp "$ROOT/install.sh" "$OLD/install.sh"
cp "$ROOT/Cargo.toml" "$OLD/Cargo.toml"
cp "$BINARY" "$OLD/target/release/desktop"
printf '%s\n' 'old skill' >"$OLD/skills/desktop-driver/SKILL.md"

cp "$ROOT/install.sh" "$ARCHIVE_ROOT/install.sh"
cp "$ROOT/Cargo.toml" "$ARCHIVE_ROOT/Cargo.toml"
cp "$ROOT/skills/desktop-driver/SKILL.md" "$ARCHIVE_ROOT/skills/desktop-driver/SKILL.md"
cp "$BINARY" "$ARCHIVE_ROOT/target/release/desktop"
tar -czf "$SCRATCH/source.tar.gz" -C "$SCRATCH/archive" "desktop-driver-$EXPECTED_VERSION"

printf '%s\n' '#!/usr/bin/env bash' 'echo "desktop 0.0.0"' >"$PREFIX/desktop"
chmod 755 "$PREFIX/desktop"
printf '%s\n' "$PREFIX/desktop" >"$STATE/desktop-driver/installed-binary"
mkdir -p "$DATA/desktop-driver/sessions/github/home"
printf '%s\n' 'login-cookie' >"$DATA/desktop-driver/sessions/github/home/cookie"

printf '%s\n' \
  '#!/usr/bin/env bash' \
  'set -eu' \
  'output=""' \
  'while [ $# -gt 0 ]; do' \
  '  if [ "$1" = "-o" ]; then output="$2"; shift 2; else shift; fi' \
  'done' \
  'cp "$DESKTOP_DRIVER_TEST_ARCHIVE" "$output"' \
  >"$FAKE_BIN/curl"
chmod 755 "$FAKE_BIN/curl"

env \
  PATH="$FAKE_BIN:$PATH" \
  XDG_STATE_HOME="$STATE" \
  XDG_DATA_HOME="$DATA" \
  DESKTOP_DRIVER_TEST_ARCHIVE="$SCRATCH/source.tar.gz" \
  "$OLD/install.sh" --update --src "$OLD" --no-build --no-agents --prefix "$PREFIX"

[ "$("$PREFIX/desktop" --version)" = "desktop $EXPECTED_VERSION" ]
cmp "$OLD/skills/desktop-driver/SKILL.md" "$ROOT/skills/desktop-driver/SKILL.md"
[ "$(cat "$DATA/desktop-driver/sessions/github/home/cookie")" = "login-cookie" ]

printf 'installer update smoke: 0.0.0 -> %s passed\n' "$EXPECTED_VERSION"
