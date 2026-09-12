#!/bin/sh
set -eu

root=$(CDPATH= cd -- "$(dirname -- "$0")/../.." && pwd -P)
exec cargo run --manifest-path "$root/Cargo.toml" --locked -q \
  -p aad-macos-ax --bin aad-macos-result-verifier -- "$@"
