#!/bin/sh

# Create a small, reproducible ustar+gzip archive without inheriting host
# ownership, timestamps, permissions, ACLs, flags, or extended attributes.
# Callers pass basename-only regular files; executable inputs become 0755 and
# all other inputs become 0644 in the archive.
create_normalized_tar_gz() {
    normalized_archive_path=$1
    normalized_archive_source_dir=$2
    shift 2

    if [ "$#" -eq 0 ]; then
        printf '%s\n' '失败：归档文件清单不能为空。' >&2
        return 1
    fi
    case $normalized_archive_path in
        ''|*/)
            printf '%s\n' '失败：归档输出路径无效。' >&2
            return 1
            ;;
    esac
    if [ -e "$normalized_archive_path" ] || [ -L "$normalized_archive_path" ]; then
        printf '%s\n' "失败：归档输出已存在：$normalized_archive_path" >&2
        return 1
    fi

    if ! normalized_archive_tar=$(command -v tar 2>/dev/null); then
        printf '%s\n' '失败：找不到 tar。' >&2
        return 1
    fi
    if ! normalized_archive_gzip=$(command -v gzip 2>/dev/null); then
        printf '%s\n' '失败：找不到 gzip。' >&2
        return 1
    fi
    if [ -z "$normalized_archive_tar" ] || [ -z "$normalized_archive_gzip" ]; then
        printf '%s\n' '失败：归档工具路径为空。' >&2
        return 1
    fi
    if ! normalized_archive_tar_version=$(
        LC_ALL=C "$normalized_archive_tar" --version 2>&1
    ); then
        printf '%s\n' '失败：无法识别 tar 实现。' >&2
        return 1
    fi
    case $normalized_archive_tar_version in
        *'GNU tar'*) normalized_archive_tar_kind=gnu ;;
        *bsdtar*|*libarchive*) normalized_archive_tar_kind=bsd ;;
        *)
            printf '%s\n' '失败：仅支持已知的 macOS bsdtar/libarchive 或 GNU tar。' >&2
            return 1
            ;;
    esac

    case $normalized_archive_path in
        */*) normalized_archive_parent=${normalized_archive_path%/*} ;;
        *) normalized_archive_parent=. ;;
    esac
    if [ -z "$normalized_archive_parent" ]; then
        normalized_archive_parent=/
    fi
    if [ ! -d "$normalized_archive_parent" ]; then
        printf '%s\n' "失败：归档输出目录不存在：$normalized_archive_parent" >&2
        return 1
    fi
    if ! normalized_archive_work=$(
        mktemp -d "$normalized_archive_parent/.normalized-archive.XXXXXX"
    ); then
        printf '%s\n' '失败：无法创建归档临时目录。' >&2
        return 1
    fi
    normalized_archive_staging=$normalized_archive_work/staging
    normalized_archive_tar_file=$normalized_archive_work/archive.tar
    normalized_archive_gzip_file=$normalized_archive_work/archive.tar.gz
    if ! chmod 700 "$normalized_archive_work" \
        || ! mkdir "$normalized_archive_staging" \
        || ! chmod 700 "$normalized_archive_staging"; then
        rm -rf "$normalized_archive_work" 2>/dev/null || :
        printf '%s\n' '失败：无法创建归档 staging 目录。' >&2
        return 1
    fi

    for normalized_archive_member do
        case $normalized_archive_member in
            ''|.|..|*/*)
                rm -rf "$normalized_archive_work" 2>/dev/null || :
                printf '%s\n' "失败：归档成员必须是安全的 basename：$normalized_archive_member" >&2
                return 1
                ;;
        esac
        normalized_archive_source=$normalized_archive_source_dir/$normalized_archive_member
        normalized_archive_destination=$normalized_archive_staging/$normalized_archive_member
        if [ ! -f "$normalized_archive_source" ] || [ -L "$normalized_archive_source" ]; then
            rm -rf "$normalized_archive_work" 2>/dev/null || :
            printf '%s\n' "失败：归档成员必须是普通文件：$normalized_archive_member" >&2
            return 1
        fi
        if ! COPYFILE_DISABLE=1 cp -p "$normalized_archive_source" \
            "$normalized_archive_destination"; then
            rm -rf "$normalized_archive_work" 2>/dev/null || :
            printf '%s\n' "失败：无法复制归档成员：$normalized_archive_member" >&2
            return 1
        fi
        if [ -x "$normalized_archive_source" ]; then
            normalized_archive_mode=755
        else
            normalized_archive_mode=644
        fi
        if ! chmod "$normalized_archive_mode" "$normalized_archive_destination"; then
            rm -rf "$normalized_archive_work" 2>/dev/null || :
            printf '%s\n' "失败：无法归一化文件权限：$normalized_archive_member" >&2
            return 1
        fi
        if ! TZ=UTC0 touch -t 200001010000 "$normalized_archive_destination"; then
            rm -rf "$normalized_archive_work" 2>/dev/null || :
            printf '%s\n' "失败：无法归一化文件时间：$normalized_archive_member" >&2
            return 1
        fi
    done

    if [ "$normalized_archive_tar_kind" = bsd ]; then
        # These flags are supported by the system bsdtar shipped with macOS.
        # COPYFILE_DISABLE also prevents AppleDouble sidecars on macOS.
        if ! COPYFILE_DISABLE=1 "$normalized_archive_tar" \
            --format ustar --uid 0 --gid 0 --uname root --gname root \
            --no-acls --no-fflags --no-xattrs --no-mac-metadata \
            -cf "$normalized_archive_tar_file" \
            -C "$normalized_archive_staging" "$@"; then
            rm -rf "$normalized_archive_work" 2>/dev/null || :
            printf '%s\n' '失败：macOS bsdtar 无法创建规范化归档。' >&2
            return 1
        fi
    else
        if ! "$normalized_archive_tar" \
            --format=ustar --owner=0 --group=0 --numeric-owner \
            --no-acls --no-selinux --no-xattrs \
            -cf "$normalized_archive_tar_file" \
            -C "$normalized_archive_staging" "$@"; then
            rm -rf "$normalized_archive_work" 2>/dev/null || :
            printf '%s\n' '失败：GNU tar 无法创建规范化归档。' >&2
            return 1
        fi
    fi

    # gzip -n omits the input filename and timestamp. Keep this separate from
    # tar so either command failing is observable under plain POSIX sh.
    if ! "$normalized_archive_gzip" -n -c "$normalized_archive_tar_file" \
        >"$normalized_archive_gzip_file"; then
        rm -rf "$normalized_archive_work" 2>/dev/null || :
        printf '%s\n' '失败：无法压缩规范化归档。' >&2
        return 1
    fi
    if ! chmod 600 "$normalized_archive_gzip_file"; then
        rm -rf "$normalized_archive_work" 2>/dev/null || :
        printf '%s\n' '失败：无法设置归档文件权限。' >&2
        return 1
    fi
    if ! mv "$normalized_archive_gzip_file" "$normalized_archive_path"; then
        rm -rf "$normalized_archive_work" 2>/dev/null || :
        printf '%s\n' '失败：无法发布结果归档。' >&2
        return 1
    fi
    if ! TZ=UTC0 touch -t 200001010000 "$normalized_archive_path"; then
        rm -f "$normalized_archive_path" 2>/dev/null || :
        rm -rf "$normalized_archive_work" 2>/dev/null || :
        printf '%s\n' '失败：无法归一化归档文件时间。' >&2
        return 1
    fi
    rm -rf "$normalized_archive_work" 2>/dev/null || :
    return 0
}
