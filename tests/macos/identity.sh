#!/bin/sh

identity_set_error() {
    IDENTITY_ATTESTATION_ERROR=$1
}

identity_value_is_single_nonempty_line() {
    identity_value=$1
    case $identity_value in
        ''|*'
'*) return 1 ;;
        *[![:space:]]*) return 0 ;;
        *) return 1 ;;
    esac
}

identity_collect_app() {
    identity_label=$1
    identity_app=$2
    identity_executable=$3
    identity_expected_identifier=$4
    identity_requirement_output=$identity_work/$identity_label.requirement
    identity_details_output=$identity_work/$identity_label.details
    identity_architectures_output=$identity_work/$identity_label.architectures
    identity_sha_output=$identity_work/$identity_label.sha256

    if ! "$identity_codesign" -d -r- "$identity_app" \
        >"$identity_requirement_output" 2>&1; then
        identity_set_error "$identity_label.requirement_read"
        printf '%s\n' "失败：无法读取 $identity_label designated requirement。" >&2
        return 1
    fi
    if ! identity_designated_requirement=$(
        sed -n 's/^designated =>[[:space:]]*//p' "$identity_requirement_output"
    ); then
        printf '%s\n' "失败：无法解析 $identity_label designated requirement。" >&2
        return 1
    fi
    if ! identity_value_is_single_nonempty_line "$identity_designated_requirement"; then
        identity_set_error "$identity_label.requirement_invalid"
        printf '%s\n' "失败：$identity_label designated requirement 为空或无效。" >&2
        return 1
    fi

    if ! "$identity_codesign" -d --verbose=4 "$identity_app" \
        >"$identity_details_output" 2>&1; then
        identity_set_error "$identity_label.details_read"
        printf '%s\n' "失败：无法读取 $identity_label 签名详情。" >&2
        return 1
    fi
    if ! identity_identifier=$(
        sed -n 's/^Identifier=//p' "$identity_details_output"
    ); then
        printf '%s\n' "失败：无法解析 $identity_label Identifier。" >&2
        return 1
    fi
    if ! identity_value_is_single_nonempty_line "$identity_identifier"; then
        identity_set_error "$identity_label.identifier_invalid"
        printf '%s\n' "失败：$identity_label Identifier 为空或无效。" >&2
        return 1
    fi
    if [ "$identity_identifier" != "$identity_expected_identifier" ]; then
        identity_set_error "$identity_label.identifier_mismatch"
        printf '%s\n' "失败：$identity_label Identifier 与固定 bundle ID 不一致。" >&2
        return 1
    fi
    if ! identity_cdhash=$(sed -n 's/^CDHash=//p' "$identity_details_output"); then
        printf '%s\n' "失败：无法解析 $identity_label CDHash。" >&2
        return 1
    fi
    if ! identity_value_is_single_nonempty_line "$identity_cdhash"; then
        identity_set_error "$identity_label.cdhash_invalid"
        printf '%s\n' "失败：$identity_label CDHash 为空或无效。" >&2
        return 1
    fi
    case $identity_cdhash in
        *[!0123456789abcdefABCDEF]*)
            identity_set_error "$identity_label.cdhash_invalid"
            printf '%s\n' "失败：$identity_label CDHash 不是十六进制。" >&2
            return 1
            ;;
    esac
    if ! identity_team_identifier=$(
        sed -n 's/^TeamIdentifier=//p' "$identity_details_output"
    ); then
        printf '%s\n' "失败：无法解析 $identity_label TeamIdentifier。" >&2
        return 1
    fi
    if [ -n "$identity_team_identifier" ] \
        && ! identity_value_is_single_nonempty_line "$identity_team_identifier"; then
        identity_set_error "$identity_label.team_identifier_invalid"
        printf '%s\n' "失败：$identity_label TeamIdentifier 无效。" >&2
        return 1
    fi

    if ! "$identity_lipo" -archs "$identity_executable" \
        >"$identity_architectures_output" 2>&1; then
        identity_set_error "$identity_label.architectures_read"
        printf '%s\n' "失败：无法读取 $identity_label architectures。" >&2
        return 1
    fi
    if ! identity_architectures=$(sed -n '1p' "$identity_architectures_output"); then
        printf '%s\n' "失败：无法解析 $identity_label architectures。" >&2
        return 1
    fi
    if ! identity_value_is_single_nonempty_line "$identity_architectures"; then
        identity_set_error "$identity_label.architectures_invalid"
        printf '%s\n' "失败：$identity_label architectures 为空或无效。" >&2
        return 1
    fi
    for identity_architecture in $identity_architectures; do
        case $identity_architecture in
            arm64|x86_64) ;;
            *)
                identity_set_error "$identity_label.architectures_unsupported"
                printf '%s\n' "失败：$identity_label architectures 包含不受支持的值。" >&2
                return 1
                ;;
        esac
    done

    if ! "$identity_shasum" -a 256 "$identity_executable" \
        >"$identity_sha_output" 2>&1; then
        identity_set_error "$identity_label.sha256_read"
        printf '%s\n' "失败：无法计算 $identity_label sha256。" >&2
        return 1
    fi
    if ! identity_sha256=$(
        sed -n '1s/[[:space:]].*$//p' "$identity_sha_output"
    ); then
        printf '%s\n' "失败：无法解析 $identity_label sha256。" >&2
        return 1
    fi
    if ! identity_value_is_single_nonempty_line "$identity_sha256" \
        || [ "${#identity_sha256}" -ne 64 ]; then
        identity_set_error "$identity_label.sha256_invalid"
        printf '%s\n' "失败：$identity_label sha256 为空或无效。" >&2
        return 1
    fi
    case $identity_sha256 in
        *[!0123456789abcdefABCDEF]*)
            identity_set_error "$identity_label.sha256_invalid"
            printf '%s\n' "失败：$identity_label sha256 不是十六进制。" >&2
            return 1
            ;;
    esac

    {
        printf '%s\n' "[$identity_label]"
        printf '%s\n' "designated => $identity_designated_requirement"
        printf '%s\n' "Identifier=$identity_identifier"
        if [ -n "$identity_team_identifier" ]; then
            printf '%s\n' "TeamIdentifier=$identity_team_identifier"
        fi
        printf '%s\n' "CDHash=$identity_cdhash"
        printf '%s\n' "architectures=$identity_architectures"
        printf '%s\n' "sha256=$identity_sha256"
    } >>"$identity_document"
}

write_identity_attestation() {
    IDENTITY_ATTESTATION_ERROR=unknown
    if [ "$#" -ne 15 ]; then
        identity_set_error arguments_invalid
        printf '%s\n' '失败：identity attestation 参数数量无效。' >&2
        return 1
    fi
    identity_output=$1
    identity_swiftc=$2
    identity_codesign=$3
    identity_lipo=$4
    identity_shasum=$5
    identity_stability=$6
    identity_source_revision=$7
    identity_source_worktree=$8
    identity_source_package_digest=$9
    shift 9
    identity_runner_app=$1
    identity_runner_executable=$2
    identity_runner_identifier=$3
    identity_fixture_app=$4
    identity_fixture_executable=$5
    identity_fixture_identifier=$6

    if ! identity_value_is_single_nonempty_line "$identity_stability"; then
        identity_set_error stability_invalid
        printf '%s\n' '失败：identity stability 为空或无效。' >&2
        return 1
    fi
    case $identity_stability in
        ephemeral|stable_identity_requested) ;;
        *)
            identity_set_error stability_unsupported
            printf '%s\n' '失败：identity stability 值不受支持。' >&2
            return 1
            ;;
    esac
    case ${#identity_source_revision} in
        40|64) ;;
        *)
            identity_set_error source_revision_invalid
            printf '%s\n' '失败：source revision 长度无效。' >&2
            return 1
            ;;
    esac
    case $identity_source_revision in
        *[!0123456789abcdef]*)
            identity_set_error source_revision_invalid
            printf '%s\n' '失败：source revision 不是小写十六进制。' >&2
            return 1
            ;;
    esac
    case $identity_source_worktree in
        clean|dirty) ;;
        *)
            identity_set_error source_worktree_invalid
            printf '%s\n' '失败：source worktree 状态无效。' >&2
            return 1
            ;;
    esac
    if [ "${#identity_source_package_digest}" -ne 64 ]; then
        identity_set_error source_digest_invalid
        printf '%s\n' '失败：source package digest 长度无效。' >&2
        return 1
    fi
    case $identity_source_package_digest in
        *[!0123456789abcdef]*)
            identity_set_error source_digest_invalid
            printf '%s\n' '失败：source package digest 不是小写十六进制。' >&2
            return 1
            ;;
    esac
    if ! identity_work=$(mktemp -d "$identity_output.work.XXXXXX"); then
        identity_set_error temporary_directory_failed
        printf '%s\n' '失败：无法创建 identity attestation 临时目录。' >&2
        return 1
    fi
    identity_document=$identity_work/identity.txt
    identity_swift_output=$identity_work/swift-version
    if ! (umask 077 && : >"$identity_document"); then
        identity_set_error document_create_failed
        rm -rf "$identity_work" 2>/dev/null || :
        printf '%s\n' '失败：无法创建 identity attestation。' >&2
        return 1
    fi

    if ! "$identity_swiftc" --version >"$identity_swift_output" 2>&1; then
        identity_set_error swift_version_read
        rm -rf "$identity_work" 2>/dev/null || :
        printf '%s\n' '失败：无法读取 swift version。' >&2
        return 1
    fi
    if ! identity_swift_version=$(sed -n '1p' "$identity_swift_output"); then
        rm -rf "$identity_work" 2>/dev/null || :
        printf '%s\n' '失败：无法解析 swift version。' >&2
        return 1
    fi
    if ! identity_value_is_single_nonempty_line "$identity_swift_version"; then
        identity_set_error swift_version_invalid
        rm -rf "$identity_work" 2>/dev/null || :
        printf '%s\n' '失败：swift version 为空或无效。' >&2
        return 1
    fi
    {
        printf '%s\n' "swift=$identity_swift_version"
        printf '%s\n' "identity_stability=$identity_stability"
        printf '%s\n' "source_revision=$identity_source_revision"
        printf '%s\n' "source_worktree=$identity_source_worktree"
        printf '%s\n' \
            "source_package_digest=$identity_source_package_digest"
    } >>"$identity_document"

    if ! identity_collect_app runner "$identity_runner_app" \
        "$identity_runner_executable" "$identity_runner_identifier"; then
        rm -rf "$identity_work" 2>/dev/null || :
        return 1
    fi
    if ! identity_collect_app fixture "$identity_fixture_app" \
        "$identity_fixture_executable" "$identity_fixture_identifier"; then
        rm -rf "$identity_work" 2>/dev/null || :
        return 1
    fi
    if ! chmod 600 "$identity_document"; then
        identity_set_error document_permissions_failed
        rm -rf "$identity_work" 2>/dev/null || :
        printf '%s\n' '失败：无法设置 identity attestation 权限。' >&2
        return 1
    fi
    if ! mv "$identity_document" "$identity_output"; then
        identity_set_error document_publish_failed
        rm -rf "$identity_work" 2>/dev/null || :
        printf '%s\n' '失败：无法发布 identity attestation。' >&2
        return 1
    fi
    rm -rf "$identity_work" 2>/dev/null || :
    return 0
}
