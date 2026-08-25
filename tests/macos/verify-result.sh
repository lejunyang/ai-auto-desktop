#!/bin/sh
set -eu

script_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd -P)
if ! python3_path=$(command -v python3 2>/dev/null) || [ -z "$python3_path" ]; then
    printf '%s\n' '{"schema_version":"ai-auto-desktop.macos-result-verifier/v1","status":"failed","archive_valid":false,"verified_archive":false,"report_passed":false,"trusted_archive":false,"source_trusted":false,"qualified":false,"error":{"code":"python_unavailable","message":"找不到 Python 3。"}}'
    exit 69
fi
exec "$python3_path" "$script_dir/verify-result.py" "$@"
