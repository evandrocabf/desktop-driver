#!/usr/bin/env bash
#
# Runs desktop-driver end to end on several Linux distributions.
#
# Nothing about a session is portable by inspection: where at-spi2-core puts its
# helpers, what the D-Bus binary is called, and which package provides Xvfb all
# differ, and every one of those is a run-time failure on somebody else's
# machine. This builds the binary once on the oldest supported glibc and then
# actually starts a display, launches a GTK application onto it, reads its
# accessibility tree and captures the screen, on each distribution in turn.
#
# Requires podman. Takes a few minutes, most of it the first build.
#
#   scripts/distro-matrix.sh            # every distribution
#   scripts/distro-matrix.sh arch       # just one

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"; podman rm -f dd-build >/dev/null 2>&1 || true' EXIT

# Debian is the build host because it has the oldest glibc here, and a binary
# built against an older glibc runs on newer ones but never the other way round.
BUILDER=docker.io/library/debian:13

# name|image|install command|packages
DISTROS=(
  "debian|docker.io/library/debian:13|apt-get update -qq && apt-get install -y -qq|at-spi2-core xvfb openbox dbus-bin zenity"
  "ubuntu|docker.io/library/ubuntu:24.04|apt-get update -qq && apt-get install -y -qq|at-spi2-core xvfb openbox dbus-bin zenity"
  "arch|docker.io/library/archlinux:latest|pacman -Sy --noconfirm >/dev/null && pacman -S --noconfirm|at-spi2-core xorg-server-xvfb openbox dbus zenity"
  "opensuse|docker.io/opensuse/tumbleweed:latest|zypper -n --gpg-auto-import-keys refresh >/dev/null && zypper -n install|at-spi2-core xorg-x11-server-Xvfb openbox dbus-1-daemon zenity"
)

only="${1:-}"

echo "==> building on ${BUILDER}"
podman run -d --name dd-build -v "$ROOT:/work:ro,Z" "$BUILDER" sleep 1800 >/dev/null
podman exec dd-build bash -c '
  set -e
  apt-get update -qq >/dev/null 2>&1
  apt-get install -y -qq curl build-essential pkg-config >/dev/null 2>&1
  curl -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal >/dev/null 2>&1
  cp -r /work /build && cd /build && rm -rf target
  PATH=/root/.cargo/bin:$PATH cargo build --release -q
' >/dev/null
podman cp dd-build:/build/target/release/desktop "$WORK/desktop"
chmod +x "$WORK/desktop"
podman rm -f dd-build >/dev/null
echo "    built $(du -h "$WORK/desktop" | cut -f1)"

failures=0
for entry in "${DISTROS[@]}"; do
  IFS='|' read -r name image installer packages <<<"$entry"
  [ -n "$only" ] && [ "$only" != "$name" ] && continue

  echo
  echo "==> $name"
  if podman run --rm -v "$WORK:/s:ro,Z" "$image" bash -c "
    set -e
    $installer $packages >/dev/null 2>&1
    cp /s/desktop /usr/local/bin/desktop
    export HOME=/root

    # Every dependency must read as installed, and nothing prescribed. The key
    # is always present, so this has to test the value: matching the key alone
    # fails on a perfectly healthy machine.
    desktop doctor --json | grep -q '\"install_command\": null' ||
      { echo '    FAIL: doctor prescribes packages that are already installed:';
        desktop doctor --json | grep install_command; exit 1; }

    desktop session start --size 800x600 >/dev/null
    desktop session run zenity --info --text='matrix probe' >/dev/null 2>&1
    sleep 5

    desktop apps | grep -q zenity ||
      { echo '    FAIL: the accessibility tree is empty'; exit 1; }
    desktop snapshot | grep -q 'matrix probe' ||
      { echo '    FAIL: the snapshot does not contain the window text'; exit 1; }
    desktop screenshot --output /tmp/shot.png >/dev/null ||
      { echo '    FAIL: capture'; exit 1; }
    [ -s /tmp/shot.png ] || { echo '    FAIL: empty screenshot'; exit 1; }
    desktop session stop >/dev/null

    echo '    ok: session, tree, snapshot, capture'
  "; then :; else
    failures=$((failures + 1))
    echo "    FAILED"
  fi
done

echo
if [ "$failures" -eq 0 ]; then
  echo "all distributions passed"
else
  echo "$failures distribution(s) failed"
  exit 1
fi
