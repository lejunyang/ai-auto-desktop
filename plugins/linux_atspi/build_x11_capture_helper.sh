#!/usr/bin/env sh
set -eu

script_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd -P)
build_dir=${AI_AUTO_DESKTOP_LINUX_ATSPI_BUILD_DIR:-"$script_dir/.build"}

if [ "$(uname -s 2>/dev/null || printf unknown)" != Linux ]; then
    printf '%s\n' 'X11 capture helper 只能在 Linux 构建。' >&2
    exit 69
fi
if ! command -v g++ >/dev/null 2>&1 || ! command -v pkg-config >/dev/null 2>&1; then
    printf '%s\n' '缺少 g++ 或 pkg-config。' >&2
    exit 70
fi
if ! pkg-config --exists x11; then
    printf '%s\n' '缺少 X11 开发包；Debian/Ubuntu 请安装 libx11-dev。' >&2
    exit 71
fi
case $build_dir in
    /*) ;;
    *)
        printf '%s\n' '构建目录必须是绝对路径。' >&2
        exit 72
        ;;
esac
if [ ! -e "$build_dir" ] && [ ! -L "$build_dir" ]; then
    build_parent=${build_dir%/*}
    [ -n "$build_parent" ] || build_parent=/
    if [ -L "$build_parent" ] || [ ! -d "$build_parent" ]; then
        printf '%s\n' '构建目录父级身份或权限不可信。' >&2
        exit 72
    fi
    mkdir -- "$build_dir"
fi
if [ -L "$build_dir" ] || [ ! -d "$build_dir" ] || [ ! -O "$build_dir" ] ||
   [ -g "$build_dir" ] || [ -k "$build_dir" ]; then
    printf '%s\n' '构建目录身份或权限不可信。' >&2
    exit 72
fi
build_mode=$(stat -c %a -- "$build_dir" 2>/dev/null || printf invalid)
case $build_mode in
    [0-7][0-7][0-7]|[0-7][0-7][0-7][0-7]) ;;
    *)
        printf '%s\n' '构建目录权限无法验证。' >&2
        exit 72
        ;;
esac
if [ $((0$build_mode & 0022)) -ne 0 ]; then
    printf '%s\n' '构建目录不能允许组或其他用户写入。' >&2
    exit 72
fi
build_dir=$(CDPATH= cd -- "$build_dir" && pwd -P)
case $build_dir in
    "$script_dir"|"$script_dir"/*) ;;
    /tmp/*) ;;
    *)
        printf '%s\n' '构建目录必须位于插件目录或受控临时目录。' >&2
        exit 72
        ;;
esac
old_ifs=$IFS
IFS=/
set -f
set -- $build_dir
set +f
IFS=$old_ifs
current=/
for component do
    [ -n "$component" ] || continue
    if [ "$current" = / ]; then
        current=/$component
    else
        current=$current/$component
    fi
    owner=$(stat -c %u -- "$current" 2>/dev/null || printf invalid)
    mode=$(stat -c %a -- "$current" 2>/dev/null || printf invalid)
    case $owner in
        ''|*[!0-9]*)
            printf '%s\n' '构建目录链身份或权限不可信。' >&2
            exit 72
            ;;
    esac
    case $mode in
        [0-7][0-7][0-7]|[0-7][0-7][0-7][0-7]) ;;
        *)
            printf '%s\n' '构建目录链身份或权限不可信。' >&2
            exit 72
            ;;
    esac
    if [ $((0$mode & 0022)) -ne 0 ]; then
        if [ "$owner" != 0 ] || [ $((0$mode & 01000)) -eq 0 ]; then
            printf '%s\n' '构建目录链可被其他用户修改。' >&2
            exit 72
        fi
    fi
done
output="$build_dir/x11_capture_helper"
if { [ -e "$output" ] || [ -L "$output" ]; } &&
   { [ -L "$output" ] || [ ! -f "$output" ] || [ ! -O "$output" ]; }; then
    printf '%s\n' '既有 helper 目标身份不可信。' >&2
    exit 72
fi
set -- $(pkg-config --cflags --libs x11)
temporary=$(mktemp "$build_dir/.x11_capture_helper.XXXXXX")
trap 'rm -f -- "$temporary"' EXIT HUP INT TERM
g++ -std=c++17 -O2 -Wall -Wextra -Werror -fPIE -D_FORTIFY_SOURCE=2 \
    "$script_dir/x11_capture_helper.cpp" -o "$temporary" "$@" \
    -pie -Wl,-z,relro,-z,now
chmod 0755 "$temporary"
mv -- "$temporary" "$output"
trap - EXIT HUP INT TERM
printf '%s\n' "已构建：$output" >&2
