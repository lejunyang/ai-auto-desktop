#!/usr/bin/env bash
set -euo pipefail

root=$(CDPATH= cd -- "$(dirname -- "$0")/../.." && pwd -P)
cd "$root"

if [ "${1:-}" = --inside-session ]; then
  unset NO_AT_BRIDGE AT_SPI_BUS_ADDRESS
  export QT_ACCESSIBILITY=1
  export QT_LINUX_ACCESSIBILITY_ALWAYS_ON=1
  fixture_log=${TMPDIR:-/tmp}/aad-rust-atspi-fixture.out
  "$AAD_ATSPI_FIXTURE_BINARY" >"$fixture_log" 2>&1 &
  fixture_pid=$!
  finish_fixture() {
    kill "$fixture_pid" 2>/dev/null || true
    wait "$fixture_pid" 2>/dev/null || true
  }
  trap finish_fixture EXIT HUP INT TERM
  # Output is mostly sent to /dev/null or jq; say which step stopped the run.
  report_failure() {
    printf 'AT-SPI fixture failed at line %s: %s\n' "$1" "$2" >&2
    sed 's/^/fixture: /' "$fixture_log" >&2 || true
  }
  trap 'report_failure "$LINENO" "$BASH_COMMAND"' ERR
  for _ in $(seq 1 50); do
    grep -q READY "$fixture_log" && break
    sleep 0.1
  done
  grep -q READY "$fixture_log"

  found=false
  for _ in $(seq 1 50); do
    if cargo run --locked -q -p aad-atspi --example driver -- list \
      | jq -e --argjson pid "$fixture_pid" '.applications[] | select(.process_id == $pid and .toolkit_name == "Qt")' >/dev/null; then
      found=true
      break
    fi
    sleep 0.1
  done
  [ "$found" = true ]
  cargo run --locked -q -p aad-atspi --example driver -- set-text "$fixture_pid" "Rust native changed" >/dev/null
  cargo run --locked -q -p aad-atspi --example driver -- snapshot "$fixture_pid" \
    | jq -e '.nodes[] | select(.name == "Qt fixture text entry" and .value == "Rust native changed")' >/dev/null
  cargo run --locked -q -p aad-atspi --example driver -- type-text "$fixture_pid" "Rust XTest 你好" >/dev/null
  cargo run --locked -q -p aad-atspi --example driver -- snapshot "$fixture_pid" \
    | jq -e '.nodes[] | select(.name == "Qt fixture XTest text entry" and .value == "Rust XTest 你好")' >/dev/null
  cargo run --locked -q -p aad-atspi --example driver -- pointer-click "$fixture_pid" >/dev/null
  cargo run --locked -q -p aad-atspi --example driver -- snapshot "$fixture_pid" \
    | jq -e '.nodes[] | select(.name == "Qt fixture status invoked")' >/dev/null
  cargo run --locked -q -p aad-atspi --example driver -- capture "$fixture_pid" \
    | jq -e '.frame.kind == "ArtifactRef" and .frame.mediaType == "image/png"' >/dev/null
  cargo build --locked -q -p aad-cli
  AAD_CONTROL_CLI="$root/target/debug/aad" cargo run --locked -q -p aad-control --example native_fixture -- "$fixture_pid"
  # The commands a person runs go through the same session, one process each.
  aad="$root/target/debug/aad"
  # Run one action; keep its answer quiet unless it fails.
  act() {
    local answer
    if ! answer=$("$aad" --compact do "$@"); then
      printf 'aad do %s:
%s
' "$*" "$answer" >&2
      return 1
    fi
  }
  # Invoke and XTest input reach Qt asynchronously; look again until the effect shows.
  eventually() {
    local filter=$1; shift
    for _ in $(seq 1 50); do
      "$aad" --compact find "$resource" "$@" | jq -e "$filter" >/dev/null && return 0
      sleep 0.1
    done
    "$aad" --compact find "$resource" "$@" >&2
    return 1
  }
  resource=$("$aad" --compact apps | jq -r --argjson pid "$fixture_pid"     '.apps[] | select(.metadata.process_id == $pid and .metadata.toolkit_name == "Qt") | .resource_id')
  [ -n "$resource" ]
  act fill "$resource" --role text --name "Qt fixture text entry" --value "CLI session fill"
  eventually '.found and .node.value == "CLI session fill"' --role text --name "Qt fixture text entry"
  act invoke "$resource" --role push_button --name "Reset Qt fixture status"
  eventually '.found' --name "Qt fixture status idle"
  # Gestures and chords go through the XTest helper's new commands.
  act click "$resource" --role push_button --name "Invoke Qt fixture button" --count 2
  eventually '.found' --name "Qt fixture status invoked"
  act key "$resource" --role text --name "Qt fixture text entry" --keys ctrl,a
  act type-text "$resource" --role text --name "Qt fixture text entry" --text "chord"
  eventually '.node.value == "chord"' --role text --name "Qt fixture text entry"
  # Recording: XRecord sees the XTest click, AT-SPI reports the text change.
  recorded=${TMPDIR:-/tmp}/aad-rust-atspi-recording.json
  "$aad" --compact record "$resource" --seconds 6 >"$recorded" &
  recorder=$!
  sleep 2
  act click "$resource" --role push_button --name "Invoke Qt fixture button"
  act fill "$resource" --role text --name "Qt fixture text entry" --value "recorded"
  wait "$recorder"
  jq -e '.' "$recorded" >&2 || cat "$recorded" >&2
  jq -e '[.steps[].action.type] | (index("click") != null) and (index("fill") != null)' "$recorded" >/dev/null
  refused=$("$aad" --compact find "$resource" --role push_button --nth 1 || true)
  printf '%s' "$refused" | jq -e '.error.code == "CONTROL.LOCATOR_UNSUPPORTED"' >/dev/null
  printf '%s\n' 'Rust Linux AT-SPI fixture passed'
  exit 0
fi

fixture_build=$(mktemp -d)
cleanup_fixture_build() { rm -rf -- "$fixture_build"; }
trap cleanup_fixture_build EXIT HUP INT TERM
g++ -std=c++17 -O2 -Wall -Wextra -Werror -fPIC -no-pie tests/linux/qt_atspi_fixture.cpp \
  -o "$fixture_build/atspi-fixture" $(pkg-config --cflags --libs Qt5Widgets)
export AAD_ATSPI_FIXTURE_BINARY="$fixture_build/atspi-fixture"

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
export GDK_BACKEND=x11
export GTK_A11Y=always
exec dbus-run-session -- "$0" --inside-session
