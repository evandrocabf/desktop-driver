#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
DESKTOP="${DESKTOP_DRIVER_BINARY:-$ROOT/target/release/desktop}"
WORK="$(mktemp -d "${TMPDIR:-/tmp}/desktop-driver-macos-e2e.XXXXXX")"
APP="$WORK/DesktopDriverFixture.app"
FIXTURE_PID=""

cleanup() {
  if [ -n "$FIXTURE_PID" ]; then
    kill "$FIXTURE_PID" 2>/dev/null || true
    wait "$FIXTURE_PID" 2>/dev/null || true
  fi
  rm -rf "$WORK"
}
trap cleanup EXIT INT TERM

test "$(uname -s)" = Darwin
test -x "$DESKTOP"
command -v jq >/dev/null
command -v swiftc >/dev/null

"$DESKTOP" --json capabilities > "$WORK/capabilities.json"
jq -e '
  [.capabilities.accessibility, .capabilities.element_actions,
   .capabilities.element_text, .capabilities.focus, .capabilities.keyboard,
   .capabilities.mouse, .capabilities.screenshots,
   .capabilities.window_screenshots]
  | all(.state == "supported")
' "$WORK/capabilities.json" >/dev/null || {
  echo "macOS E2E requires Accessibility, Screen Recording and Post Events grants" >&2
  jq . "$WORK/capabilities.json" >&2
  exit 1
}

mkdir -p "$APP/Contents/MacOS"
cp "$ROOT/tests/fixtures/macos/Info.plist" "$APP/Contents/Info.plist"
swiftc "$ROOT/tests/fixtures/macos/DesktopDriverFixture.swift" \
  -framework AppKit \
  -o "$APP/Contents/MacOS/DesktopDriverFixture"
"$APP/Contents/MacOS/DesktopDriverFixture" >"$WORK/fixture.log" 2>&1 &
FIXTURE_PID=$!

"$DESKTOP" --json wait --app DesktopDriverFixture \
  --role textbox --name Input --timeout 15000 >/dev/null
"$DESKTOP" --json snapshot --app DesktopDriverFixture > "$WORK/snapshot.json"
INPUT_ID="$(jq -er '.elements[] | select(.name == "Input") | .id' "$WORK/snapshot.json" | head -n 1)"

# Direct AXValue write plus exact read-back.
"$DESKTOP" --json type --element "$INPUT_ID" 'direct ✓' >/dev/null
"$DESKTOP" --json snapshot --app DesktopDriverFixture > "$WORK/snapshot.json"
jq -e '.elements[] | select(.name == "Input" and .value == "direct ✓")' \
  "$WORK/snapshot.json" >/dev/null

# Resolve the same saved element after a fresh process, then invoke AXPress.
"$DESKTOP" --json click --role button --name Commit --via action >/dev/null
"$DESKTOP" --json wait --app DesktopDriverFixture --text 'Result: direct ✓' \
  --timeout 10000 >/dev/null

# Foreground input: pointer focus, layout-independent character shortcut and
# non-BMP Unicode typing. The dedicated runner owns this desktop.
"$DESKTOP" --json snapshot --app DesktopDriverFixture > "$WORK/snapshot.json"
X="$(jq -er '.elements[] | select(.name == "Input") | .bounds | .x + 15 | floor' "$WORK/snapshot.json" | head -n 1)"
Y="$(jq -er '.elements[] | select(.name == "Input") | .bounds | .y + (.height / 2) | floor' "$WORK/snapshot.json" | head -n 1)"
"$DESKTOP" --json focus --app DesktopDriverFixture >/dev/null
"$DESKTOP" --json click --x "$X" --y "$Y" >/dev/null
"$DESKTOP" --json key 'accel+a' >/dev/null
"$DESKTOP" --json type 'Olá 😀' >/dev/null
"$DESKTOP" --json wait --app DesktopDriverFixture --text 'Olá 😀' --timeout 10000 >/dev/null

# A true double-click carries kCGMouseEventClickState=2 and selects the word;
# two unrelated single clicks leave only a caret and make this assertion fail.
"$DESKTOP" --json snapshot --app DesktopDriverFixture > "$WORK/snapshot.json"
INPUT_ID="$(jq -er '.elements[] | select(.name == "Input") | .id' "$WORK/snapshot.json" | head -n 1)"
"$DESKTOP" --json type --element "$INPUT_ID" 'doubleclick' >/dev/null
"$DESKTOP" --json focus --app DesktopDriverFixture >/dev/null
"$DESKTOP" --json click --x "$X" --y "$Y" --count 2 >/dev/null
"$DESKTOP" --json type 'Z' >/dev/null
"$DESKTOP" --json wait --app DesktopDriverFixture --text 'Z' --timeout 10000 >/dev/null
"$DESKTOP" --json snapshot --app DesktopDriverFixture > "$WORK/snapshot.json"
jq -e '.elements[] | select(.name == "Input" and .value == "Z")' \
  "$WORK/snapshot.json" >/dev/null

"$DESKTOP" --json windows --app DesktopDriverFixture > "$WORK/windows.json"
WINDOW_ID="$(jq -er '.windows[0].id' "$WORK/windows.json")"
"$DESKTOP" --json screenshot --app DesktopDriverFixture --output "$WORK/app.png" > "$WORK/app-shot.json"
"$DESKTOP" --json screenshot --window "$WINDOW_ID" --output "$WORK/window.png" > "$WORK/window-shot.json"
"$DESKTOP" --json screenshot --output "$WORK/screen.png" > "$WORK/screen-shot.json"
test -s "$WORK/app.png"
test -s "$WORK/window.png"
test -s "$WORK/screen.png"

# ScreenCaptureKit dimensions are pixels while its display/frame dimensions
# are points. Compare against the actual CG main-display mode so a Retina
# regression cannot pass merely because a PNG was produced.
read -r EXPECTED_WIDTH EXPECTED_HEIGHT EXPECTED_SCALE <<EOF
$(swift -e 'import CoreGraphics; let id = CGMainDisplayID(); let mode = CGDisplayCopyDisplayMode(id)!; print(mode.pixelWidth, mode.pixelHeight, Double(mode.pixelWidth) / Double(mode.width))')
EOF
jq -e --argjson width "$EXPECTED_WIDTH" --argjson height "$EXPECTED_HEIGHT" \
  --argjson scale "$EXPECTED_SCALE" \
  '.width == $width and .height == $height and ((.scale_factor - $scale) | fabs) < 0.01' \
  "$WORK/screen-shot.json" >/dev/null
jq -se --argjson scale "$EXPECTED_SCALE" \
  'all(.[]; ((.scale_factor - $scale) | fabs) < 0.01)' \
  "$WORK/app-shot.json" "$WORK/window-shot.json" >/dev/null

echo "macOS accessibility, focus, input and capture E2E passed"
