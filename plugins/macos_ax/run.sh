#!/bin/sh
set -eu

script_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
PYTHONPATH=$script_dir/../../src${PYTHONPATH:+:$PYTHONPATH}
export PYTHONPATH
exec "${PYTHON:-python3}" "$script_dir/macos_ax_driver.py" "$@"
