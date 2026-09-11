#!/usr/bin/env sh
set -eu

SCRIPT_DIR=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
PYTHONPATH=$SCRIPT_DIR/../../src${PYTHONPATH:+:$PYTHONPATH}
export PYTHONPATH
exec python3 "$SCRIPT_DIR/linux_atspi_driver.py" "$@"
