#!/bin/sh

source_sha256_file() {
    source_sha_path=$1
    if [ -x /usr/bin/shasum ]; then
        source_sha_tool=/usr/bin/shasum
        if ! source_sha_output=$(
            "$source_sha_tool" -a 256 "$source_sha_path" 2>/dev/null
        ); then
            return 1
        fi
    elif source_sha_tool=$(command -v shasum 2>/dev/null) \
        && [ -n "$source_sha_tool" ]; then
        if ! source_sha_output=$(
            "$source_sha_tool" -a 256 "$source_sha_path" 2>/dev/null
        ); then
            return 1
        fi
    elif source_sha_tool=$(command -v sha256sum 2>/dev/null) \
        && [ -n "$source_sha_tool" ]; then
        if ! source_sha_output=$(
            "$source_sha_tool" "$source_sha_path" 2>/dev/null
        ); then
            return 1
        fi
    else
        return 1
    fi
    source_sha_digest=${source_sha_output%%[[:space:]]*}
    if [ "${#source_sha_digest}" -ne 64 ]; then
        return 1
    fi
    case $source_sha_digest in
        *[!0123456789abcdef]*) return 1 ;;
    esac
    printf '%s\n' "$source_sha_digest"
}

source_revision_is_valid() {
    case ${#1} in
        40|64) ;;
        *) return 1 ;;
    esac
    case $1 in
        *[!0123456789abcdef]*) return 1 ;;
    esac
}

source_manifest_member_count() {
    source_count_manifest=$1
    source_count=0
    while IFS= read -r source_count_member \
        || [ -n "$source_count_member" ]; do
        case $source_count_member in
            ''|'#'*) continue ;;
        esac
        source_count=$((source_count + 1))
    done <"$source_count_manifest"
    printf '%s\n' "$source_count"
}

source_git_metadata() {
    source_git_dir=$1
    source_git_members=$2
    if ! source_git=$(command -v git 2>/dev/null) \
        || [ -z "$source_git" ]; then
        printf '%s\n' '失败：无法读取源码 revision（缺少 git）。' >&2
        return 1
    fi
    if ! SOURCE_REVISION=$(
        "$source_git" -C "$source_git_dir" rev-parse --verify 'HEAD^{commit}' \
            2>/dev/null
    ) || ! source_revision_is_valid "$SOURCE_REVISION"; then
        printf '%s\n' '失败：无法读取有效的 Git HEAD commit SHA。' >&2
        return 1
    fi
    SOURCE_WORKTREE=clean
    while IFS= read -r source_git_member \
        || [ -n "$source_git_member" ]; do
        case $source_git_member in
            ''|'#'*) continue ;;
            ''|.|..|*/*)
                printf '%s\n' '失败：源码包成员清单包含非 basename。' >&2
                return 1
                ;;
        esac
        if ! source_git_status=$(
            "$source_git" -C "$source_git_dir" status --porcelain \
                --untracked-files=all -- "$source_git_member" 2>/dev/null
        ); then
            printf '%s\n' \
                "失败：无法读取源码成员 Git 状态：$source_git_member" >&2
            return 1
        fi
        if [ -n "$source_git_status" ]; then
            SOURCE_WORKTREE=dirty
        fi
    done <"$source_git_members"
    return 0
}

generate_source_manifest() {
    source_generated_output=$1
    source_generated_dir=$2
    source_generated_members=$3
    source_generated_revision=$4
    source_generated_worktree=$5

    if ! source_revision_is_valid "$source_generated_revision"; then
        printf '%s\n' '失败：源码 revision 格式无效。' >&2
        return 1
    fi
    case $source_generated_worktree in
        clean|dirty) ;;
        *)
            printf '%s\n' '失败：源码 worktree 状态无效。' >&2
            return 1
            ;;
    esac
    if ! source_generated_count=$(
        source_manifest_member_count "$source_generated_members"
    ); then
        return 1
    fi
    source_generated_temporary=$source_generated_output.writing-$$
    rm -f "$source_generated_temporary"
    if ! (umask 077 && {
        printf '%s\n' \
            'schema_version=ai-auto-desktop.macos-source-manifest/v1' \
            "source_revision=$source_generated_revision" \
            "source_worktree=$source_generated_worktree" \
            "member_count=$source_generated_count"
    } >"$source_generated_temporary"); then
        rm -f "$source_generated_temporary"
        return 1
    fi

    while IFS= read -r source_generated_member \
        || [ -n "$source_generated_member" ]; do
        case $source_generated_member in
            ''|'#'*) continue ;;
            ''|.|..|*/*)
                rm -f "$source_generated_temporary"
                printf '%s\n' '失败：源码包成员清单包含非 basename。' >&2
                return 1
                ;;
        esac
        source_generated_path=$source_generated_dir/$source_generated_member
        if [ ! -f "$source_generated_path" ] \
            || [ -L "$source_generated_path" ]; then
            rm -f "$source_generated_temporary"
            printf '%s\n' \
                "失败：源码包成员必须是普通文件：$source_generated_member" >&2
            return 1
        fi
        if [ -x "$source_generated_path" ]; then
            source_generated_mode=0755
        else
            source_generated_mode=0644
        fi
        if ! source_generated_sha=$(
            source_sha256_file "$source_generated_path"
        ); then
            rm -f "$source_generated_temporary"
            printf '%s\n' \
                "失败：无法计算源码成员 SHA-256：$source_generated_member" >&2
            return 1
        fi
        if ! printf '%s\n' \
            "file=$source_generated_mode:$source_generated_sha:$source_generated_member" \
            >>"$source_generated_temporary"; then
            rm -f "$source_generated_temporary"
            return 1
        fi
    done <"$source_generated_members"
    if ! chmod 600 "$source_generated_temporary" \
        || ! mv "$source_generated_temporary" "$source_generated_output"; then
        rm -f "$source_generated_temporary"
        return 1
    fi
}

verify_source_manifest() {
    source_verified_manifest=$1
    source_verified_dir=$2
    source_verified_members=$3
    if [ ! -f "$source_verified_manifest" ] \
        || [ -L "$source_verified_manifest" ]; then
        printf '%s\n' '失败：SOURCE_MANIFEST.txt 缺失或不是普通文件。' >&2
        return 1
    fi
    source_verified_schema=$(sed -n '1p' "$source_verified_manifest")
    source_verified_revision_line=$(sed -n '2p' "$source_verified_manifest")
    source_verified_worktree_line=$(sed -n '3p' "$source_verified_manifest")
    if [ "$source_verified_schema" \
        != 'schema_version=ai-auto-desktop.macos-source-manifest/v1' ]; then
        printf '%s\n' '失败：源码 manifest schema 无效。' >&2
        return 1
    fi
    case $source_verified_revision_line in
        source_revision=*)
            SOURCE_REVISION=${source_verified_revision_line#source_revision=}
            ;;
        *) return 1 ;;
    esac
    if ! source_revision_is_valid "$SOURCE_REVISION"; then
        printf '%s\n' '失败：源码 manifest revision 无效。' >&2
        return 1
    fi
    case $source_verified_worktree_line in
        source_worktree=clean) SOURCE_WORKTREE=clean ;;
        source_worktree=dirty) SOURCE_WORKTREE=dirty ;;
        *)
            printf '%s\n' '失败：源码 manifest worktree 状态无效。' >&2
            return 1
            ;;
    esac

    if ! source_verified_work=$(mktemp -d \
        "${TMPDIR:-/tmp}/macos-source-verify.XXXXXX"); then
        return 1
    fi
    source_verified_expected=$source_verified_work/SOURCE_MANIFEST.txt
    if ! generate_source_manifest "$source_verified_expected" \
        "$source_verified_dir" "$source_verified_members" \
        "$SOURCE_REVISION" "$SOURCE_WORKTREE"; then
        rm -rf "$source_verified_work" 2>/dev/null || :
        return 1
    fi
    if ! cmp -s "$source_verified_manifest" "$source_verified_expected"; then
        rm -rf "$source_verified_work" 2>/dev/null || :
        printf '%s\n' \
            '失败：源码内容与 SOURCE_MANIFEST.txt 不一致。' >&2
        return 1
    fi
    rm -rf "$source_verified_work" 2>/dev/null || :
    if ! SOURCE_PACKAGE_DIGEST=$(
        source_sha256_file "$source_verified_manifest"
    ); then
        printf '%s\n' '失败：无法计算源码 manifest SHA-256。' >&2
        return 1
    fi
    return 0
}

load_source_provenance() {
    source_loaded_dir=$1
    source_loaded_members=$2
    source_loaded_manifest=$source_loaded_dir/SOURCE_MANIFEST.txt
    SOURCE_MANIFEST_PATH=
    SOURCE_MANIFEST_TEMPORARY=false
    source_loaded_is_repository=false
    if source_loaded_git=$(command -v git 2>/dev/null) \
        && [ -n "$source_loaded_git" ] \
        && source_loaded_root=$(
            "$source_loaded_git" -C "$source_loaded_dir" \
                rev-parse --show-toplevel 2>/dev/null
        ) \
        && [ -d "$source_loaded_root/tests/macos" ] \
        && source_loaded_expected=$(
            CDPATH= cd -- "$source_loaded_root/tests/macos" && pwd -P
        ) \
        && source_loaded_actual=$(
            CDPATH= cd -- "$source_loaded_dir" && pwd -P
        ) \
        && [ "$source_loaded_expected" = "$source_loaded_actual" ]; then
        source_loaded_is_repository=true
    fi

    if [ "$source_loaded_is_repository" != true ] \
        && [ -f "$source_loaded_manifest" ] \
        && [ ! -L "$source_loaded_manifest" ]; then
        if ! verify_source_manifest "$source_loaded_manifest" \
            "$source_loaded_dir" "$source_loaded_members"; then
            return 1
        fi
        SOURCE_MANIFEST_PATH=$source_loaded_manifest
        return 0
    fi

    # Direct repository runs have no generated file. Derive the exact same
    # canonical manifest in a private temporary directory from Git HEAD and
    # the current whitelisted files. Extracted source kits have no repository
    # metadata, so a missing injected manifest still fails closed there.
    if [ "$source_loaded_is_repository" != true ] \
        || ! source_git_metadata "$source_loaded_dir" "$source_loaded_members"; then
        printf '%s\n' \
            '失败：源码包缺少已注入的 SOURCE_MANIFEST.txt。' >&2
        return 1
    fi
    if ! source_loaded_work=$(mktemp -d \
        "${TMPDIR:-/tmp}/macos-source-derived.XXXXXX"); then
        return 1
    fi
    SOURCE_MANIFEST_PATH=$source_loaded_work/SOURCE_MANIFEST.txt
    SOURCE_MANIFEST_TEMPORARY=true
    if ! generate_source_manifest "$SOURCE_MANIFEST_PATH" \
        "$source_loaded_dir" "$source_loaded_members" \
        "$SOURCE_REVISION" "$SOURCE_WORKTREE" \
        || ! SOURCE_PACKAGE_DIGEST=$(
            source_sha256_file "$SOURCE_MANIFEST_PATH"
        ); then
        rm -rf "$source_loaded_work" 2>/dev/null || :
        SOURCE_MANIFEST_PATH=
        SOURCE_MANIFEST_TEMPORARY=false
        return 1
    fi
    return 0
}

release_source_provenance() {
    if [ "${SOURCE_MANIFEST_TEMPORARY:-false}" = true ] \
        && [ -n "${SOURCE_MANIFEST_PATH:-}" ]; then
        rm -rf "${SOURCE_MANIFEST_PATH%/*}" 2>/dev/null || :
    fi
    SOURCE_MANIFEST_PATH=
    SOURCE_MANIFEST_TEMPORARY=false
}
