#!/usr/bin/env bash
# Desktop parity on Linux: capture, Tesseract OCR, paste, clipboard, global
# keys, EWMH window actions and launch, each checked by its observable effect
# against the Qt fixture, in a private Xvfb + D-Bus session with openbox as
# the EWMH window manager. Needs xvfb xauth dbus-x11 at-spi2-core openbox
# tesseract-ocr tesseract-ocr-eng xclip qtbase5-dev, and the X11 helpers built
# (plugins/linux_atspi/build_x11_*_helper.sh).
set -euo pipefail

root=$(CDPATH= cd -- "$(dirname -- "$0")/../.." && pwd -P)
cd "$root"

if [ "${1:-}" = --inside-session ]; then
  unset NO_AT_BRIDGE AT_SPI_BUS_ADDRESS
  export QT_ACCESSIBILITY=1
  export QT_LINUX_ACCESSIBILITY_ALWAYS_ON=1
  openbox >"${TMPDIR:-/tmp}/aad-linux-parity-openbox.out" 2>&1 &
  openbox_pid=$!
  for _ in $(seq 1 50); do
    xprop -root _NET_SUPPORTING_WM_CHECK 2>/dev/null | grep -q 'window id' && break
    sleep 0.1
  done
  fixture_log=${TMPDIR:-/tmp}/aad-linux-parity-fixture.out
  "$AAD_ATSPI_FIXTURE_BINARY" >"$fixture_log" 2>&1 &
  fixture_pid=$!
  finish() {
    kill "$fixture_pid" "$openbox_pid" 2>/dev/null || true
    # The clipboard restorer and launched copies live in this session only.
    pkill -f "$AAD_ATSPI_FIXTURE_BINARY" 2>/dev/null || true
    wait 2>/dev/null || true
  }
  trap finish EXIT HUP INT TERM
  trap 'printf "parity fixture failed at line %s: %s\n" "$LINENO" "$BASH_COMMAND" >&2; sed "s/^/fixture: /" "$fixture_log" >&2 || true' ERR
  for _ in $(seq 1 200); do
    grep -q READY "$fixture_log" && break
    sleep 0.1
  done
  grep -q READY "$fixture_log"
  found=false
  for _ in $(seq 1 50); do
    if cargo run --locked -q -p aad-atspi --example driver -- list \
      | jq -e --argjson pid "$fixture_pid" '.applications[] | select(.process_id == $pid)' >/dev/null; then
      found=true
      break
    fi
    sleep 0.1
  done
  [ "$found" = true ]
  cargo run --locked -q -p aad-control --example linux_parity -- "$fixture_pid" "$AAD_ATSPI_FIXTURE_BINARY"
  exit 0
fi

fixture_build=$(mktemp -d)
g++ -std=c++17 -O2 -Wall -Wextra -Werror -fPIC -no-pie tests/linux/qt_atspi_fixture.cpp \
  -o "$fixture_build/atspi-fixture" $(pkg-config --cflags --libs Qt5Widgets)
chmod 0755 "$fixture_build" "$fixture_build/atspi-fixture"
export AAD_ATSPI_FIXTURE_BINARY="$fixture_build/atspi-fixture"

display_number=${AAD_PARITY_DISPLAY:-109}
authority=$(mktemp)
xauth -f "$authority" add ":$display_number" . "$(od -An -N16 -tx1 /dev/urandom | tr -d ' \n')"
Xvfb ":$display_number" -screen 0 1280x800x24 -auth "$authority" \
  >"${TMPDIR:-/tmp}/aad-linux-parity-xvfb.out" 2>&1 &
xvfb_pid=$!
cleanup() {
  kill "$xvfb_pid" 2>/dev/null || true
  rm -f -- "$authority"
  rm -rf -- "$fixture_build"
}
trap cleanup EXIT HUP INT TERM
for _ in $(seq 1 50); do
  [ -S "/tmp/.X11-unix/X$display_number" ] && break
  sleep 0.1
done

export DISPLAY=":$display_number"
export XAUTHORITY="$authority"
export XDG_SESSION_TYPE=x11
export XDG_CURRENT_DESKTOP=KDE
dbus-run-session -- "$0" --inside-session
