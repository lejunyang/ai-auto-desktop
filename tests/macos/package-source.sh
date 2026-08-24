#!/bin/sh
set -eu

script_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd -P)
manifest=$script_dir/SOURCE_PACKAGE_FILES.txt

if [ "$#" -ne 1 ]; then
    printf '%s\n' '用法：./package-source.sh OUTPUT.tar.gz' >&2
    exit 64
fi
output_path=$1

. "$script_dir/archive.sh"

set --
while IFS= read -r source_member || [ -n "$source_member" ]; do
    case $source_member in
        ''|'#'*) continue ;;
    esac
    set -- "$@" "$source_member"
done <"$manifest"

if ! create_normalized_tar_gz "$output_path" "$script_dir" "$@"; then
    printf '%s\n' '失败：无法创建 macOS testkit 源码包。' >&2
    exit 1
fi
printf '%s\n' "源码包：$output_path" >&2
