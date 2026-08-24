#!/usr/bin/env sh
set -eu

script_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd -P)
build_dir=${AI_AUTO_DESKTOP_LINUX_ATSPI_BUILD_DIR:-"$script_dir/.build"}
output="$build_dir/x11_xtest_helper"

if [ "$(uname -s 2>/dev/null || printf unknown)" != Linux ]; then
    printf '%s\n' 'XTest helper 只能在 Linux 构建。' >&2
    exit 69
fi
if ! command -v g++ >/dev/null 2>&1 || ! command -v pkg-config >/dev/null 2>&1; then
    printf '%s\n' '缺少 g++ 或 pkg-config。' >&2
    exit 70
fi
if ! pkg-config --exists x11 xtst; then
    printf '%s\n' '缺少 X11/XTest 开发包；Debian/Ubuntu 请安装 libx11-dev libxtst-dev。' >&2
    exit 71
fi
mkdir -p "$build_dir"
set -- $(pkg-config --cflags --libs x11 xtst)
g++ -std=c++17 -O2 -Wall -Wextra -Werror \
    "$script_dir/x11_xtest_helper.cpp" -o "$output" "$@"
chmod 0755 "$output"
printf '%s\n' "已构建：$output" >&2
