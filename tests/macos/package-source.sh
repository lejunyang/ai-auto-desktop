#!/bin/sh
set -eu

script_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd -P)
manifest=$script_dir/SOURCE_PACKAGE_FILES.txt

usage() {
    printf '%s\n' \
        '用法：./package-source.sh [--allow-dirty] OUTPUT.tar.gz' >&2
}

allow_dirty=false
if [ "$#" -eq 2 ] && [ "$1" = --allow-dirty ]; then
    allow_dirty=true
    shift
fi
if [ "$#" -ne 1 ]; then
    usage
    exit 64
fi
output_path=$1

. "$script_dir/archive.sh"
. "$script_dir/source-provenance.sh"

if ! source_git_metadata "$script_dir" "$manifest"; then
    exit 1
fi
if [ "$SOURCE_WORKTREE" = dirty ] && [ "$allow_dirty" != true ]; then
    printf '%s\n' \
        '失败：源码包白名单文件有未提交改动；发布包默认要求 clean Git tree。' \
        '开发验证可显式传入 --allow-dirty，manifest 会如实标记 dirty。' >&2
    exit 65
fi

case $output_path in
    */*) output_parent=${output_path%/*} ;;
    *) output_parent=. ;;
esac
if [ -z "$output_parent" ]; then
    output_parent=/
fi
if [ ! -d "$output_parent" ]; then
    printf '%s\n' "失败：源码包输出目录不存在：$output_parent" >&2
    exit 1
fi
if ! package_work=$(mktemp -d "$output_parent/.macos-source.XXXXXX"); then
    printf '%s\n' '失败：无法创建源码包临时目录。' >&2
    exit 1
fi
package_staging=$package_work/staging
if ! mkdir "$package_staging" || ! chmod 700 "$package_staging"; then
    rm -rf "$package_work" 2>/dev/null || :
    exit 1
fi
trap 'rm -rf "$package_work" 2>/dev/null || :' EXIT HUP INT TERM

set --
while IFS= read -r source_member || [ -n "$source_member" ]; do
    case $source_member in
        ''|'#'*) continue ;;
    esac
    if [ ! -f "$script_dir/$source_member" ] \
        || [ -L "$script_dir/$source_member" ]; then
        printf '%s\n' \
            "失败：源码包成员必须是普通文件：$source_member" >&2
        exit 1
    fi
    if ! COPYFILE_DISABLE=1 cp -p "$script_dir/$source_member" \
        "$package_staging/$source_member"; then
        printf '%s\n' "失败：无法 stage 源码包成员：$source_member" >&2
        exit 1
    fi
    if [ -x "$script_dir/$source_member" ]; then
        chmod 755 "$package_staging/$source_member"
    else
        chmod 644 "$package_staging/$source_member"
    fi
    set -- "$@" "$source_member"
done <"$manifest"

source_manifest_path=$package_staging/SOURCE_MANIFEST.txt
if ! generate_source_manifest "$source_manifest_path" "$package_staging" \
    "$package_staging/SOURCE_PACKAGE_FILES.txt" \
    "$SOURCE_REVISION" "$SOURCE_WORKTREE"; then
    printf '%s\n' '失败：无法生成源码内容 manifest。' >&2
    exit 1
fi
chmod 644 "$source_manifest_path"
if ! SOURCE_PACKAGE_DIGEST=$(source_sha256_file "$source_manifest_path"); then
    printf '%s\n' '失败：无法计算源码内容摘要。' >&2
    exit 1
fi
set -- "$@" SOURCE_MANIFEST.txt

if ! create_normalized_tar_gz "$output_path" "$package_staging" "$@"; then
    printf '%s\n' '失败：无法创建 macOS testkit 源码包。' >&2
    exit 1
fi
if ! source_archive_sha256=$(source_sha256_file "$output_path"); then
    rm -f "$output_path"
    printf '%s\n' '失败：无法计算源码归档 SHA-256。' >&2
    exit 1
fi
printf '%s\n' "源码包：$output_path" >&2
printf '%s\n' "源码 revision：$SOURCE_REVISION" >&2
printf '%s\n' "源码 worktree：$SOURCE_WORKTREE" >&2
printf '%s\n' "源码内容 SHA-256：$SOURCE_PACKAGE_DIGEST" >&2
printf '%s\n' "源码归档 SHA-256：$source_archive_sha256" >&2
