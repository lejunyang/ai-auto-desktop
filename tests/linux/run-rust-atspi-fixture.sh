#!/usr/bin/env bash
set -euo pipefail

root=$(CDPATH= cd -- "$(dirname -- "$0")/../.." && pwd -P)
cd "$root"

if [ "${1:-}" = --inside-session ]; then
  unset NO_AT_BRIDGE AT_SPI_BUS_ADDRESS
  fixture_log=${TMPDIR:-/tmp}/aad-rust-atspi-fixture.out
  python3 tests/linux/atspi_fixture_app.py >"$fixture_log" 2>&1 &
  fixture_pid=$!
  finish_fixture() {
    kill "$fixture_pid" 2>/dev/null || true
    wait "$fixture_pid" 2>/dev/null || true
  }
  trap finish_fixture EXIT HUP INT TERM
  for _ in $(seq 1 50); do
    grep -q READY "$fixture_log" && break
    sleep 0.1
  done
  grep -q READY "$fixture_log"

  cargo run --locked -q -p aad-atspi --example driver -- list \
    | jq -e --argjson pid "$fixture_pid" '.applications[] | select(.process_id == $pid and .toolkit_name == "gtk")' >/dev/null
  cargo run --locked -q -p aad-atspi --example driver -- set-text "$fixture_pid" "Rust native changed" >/dev/null
  cargo run --locked -q -p aad-atspi --example driver -- snapshot "$fixture_pid" \
    | jq -e '.nodes[] | select(.name == "Fixture text entry" and .value == "Rust native changed")' >/dev/null
  cargo run --locked -q -p aad-atspi --example driver -- type-text "$fixture_pid" "Rust XTest 你好" >/dev/null
  cargo run --locked -q -p aad-atspi --example driver -- snapshot "$fixture_pid" \
    | jq -e '.nodes[] | select(.name == "Fixture XTest text entry" and .value == "Rust XTest 你好")' >/dev/null
  cargo run --locked -q -p aad-atspi --example driver -- pointer-click "$fixture_pid" >/dev/null
  cargo run --locked -q -p aad-atspi --example driver -- snapshot "$fixture_pid" \
    | jq -e '.nodes[] | select(.name == "Fixture status invoked")' >/dev/null
  cargo run --locked -q -p aad-atspi --example driver -- toggle "$fixture_pid" >/dev/null
  cargo run --locked -q -p aad-atspi --example driver -- expand "$fixture_pid" >/dev/null
  cargo run --locked -q -p aad-atspi --example driver -- collapse "$fixture_pid" >/dev/null
  cargo run --locked -q -p aad-atspi --example driver -- capture "$fixture_pid" \
    | jq -e '.frame.kind == "ArtifactRef" and .frame.mediaType == "image/png"' >/dev/null
  printf '%s\n' 'Rust Linux AT-SPI fixture passed'
  exit 0
fi

display_number=108
authority=$(mktemp)
cookie=0123456789abcdef0123456789abcdef
xauth -f "$authority" add ":$display_number" . "$cookie"
Xvfb ":$display_number" -screen 0 1024x768x24 -auth "$authority" \
  >"${TMPDIR:-/tmp}/aad-rust-atspi-xvfb.out" \
  2>"${TMPDIR:-/tmp}/aad-rust-atspi-xvfb.err" &
xvfb_pid=$!
cleanup() {
  kill "$xvfb_pid" 2>/dev/null || true
  rm -f -- "$authority"
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
export GDK_BACKEND=x11
export GTK_A11Y=always
exec dbus-run-session -- "$0" --inside-session
