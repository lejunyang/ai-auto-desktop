#!/bin/sh
set -eu

script_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd -P)
. "$script_dir/archive.sh"
. "$script_dir/source-provenance.sh"
build_root="$script_dir/.build"
output_root="$script_dir/results"
prompt_accessibility=false
runner_timeout_seconds=30
runner_pid=
watchdog_pid=
runner_pid_observed=false
source_revision=unavailable
source_worktree=unavailable
source_package_digest=unavailable
source_manifest_path=
source_verified=false
identity_available_this_run=false

usage() {
    cat >&2 <<'EOF'
用法：tests/macos/run.sh [--prompt-accessibility] [--timeout SECONDS] [--output DIR] [--build-dir DIR]

默认只检查 Accessibility trust，不弹授权提示。只有显式传入
--prompt-accessibility 才会调用带 prompt 的系统 API。套件绝不请求截屏授权，也不截图。
runner 默认最多运行 30 秒；--timeout 只接受 1 到 600 的整数秒。
运行前会验证 SOURCE_MANIFEST.txt；结果携带源码 revision 和源码内容摘要。
EOF
}

while [ "$#" -gt 0 ]; do
    case $1 in
        --prompt-accessibility)
            prompt_accessibility=true
            shift
            ;;
        --timeout)
            [ "$#" -ge 2 ] || { usage; exit 64; }
            case $2 in
                ''|*[!0-9]*) usage; exit 64 ;;
            esac
            if [ "$2" -lt 1 ] || [ "$2" -gt 600 ]; then
                usage
                exit 64
            fi
            runner_timeout_seconds=$2
            shift 2
            ;;
        --output)
            [ "$#" -ge 2 ] || { usage; exit 64; }
            output_root=$2
            shift 2
            ;;
        --build-dir)
            [ "$#" -ge 2 ] || { usage; exit 64; }
            build_root=$2
            shift 2
            ;;
        -h|--help)
            usage
            exit 0
            ;;
        *)
            usage
            exit 64
            ;;
    esac
done

arch=$(uname -m 2>/dev/null || printf unknown)
case $arch in
    arm64|x86_64) ;;
    *) arch=unknown ;;
esac
timestamp=$(date -u '+%Y%m%dT%H%M%SZ')
result_dir="$output_root/$timestamp-$arch-$$"
report_path="$result_dir/report.json"
archive_path="$result_dir/macos-ax-test-result.tar.gz"
if [ -L "$output_root" ]; then
    printf '%s\n' '失败：输出根目录不能是符号链接。' >&2
    exit 1
fi
mkdir -p "$output_root"
if [ -L "$build_root" ]; then
    printf '%s\n' '失败：构建目录不能是符号链接。' >&2
    exit 1
fi
if [ -e "$result_dir" ] || [ -L "$result_dir" ]; then
    printf '%s\n' '失败：结果目录已存在。' >&2
    exit 1
fi
mkdir "$result_dir"
chmod 700 "$result_dir"

if load_source_provenance "$script_dir" \
    "$script_dir/SOURCE_PACKAGE_FILES.txt"; then
    source_revision=$SOURCE_REVISION
    source_worktree=$SOURCE_WORKTREE
    source_package_digest=$SOURCE_PACKAGE_DIGEST
    source_manifest_path=$SOURCE_MANIFEST_PATH
    source_verified=true
    release_source_provenance
else
    write_provenance_error=invalid_source_provenance
fi

run_with_watchdog() {
    timeout_marker="$result_dir/.runner-timeout"
    runner_pid_file="$result_dir/.runner-pid"
    cancel_file="$result_dir/.runner-cancel"
    rm -f "$timeout_marker"
    rm -f "$runner_pid_file"
    rm -f "$cancel_file"
    /usr/bin/open -n -W "$runner_app" --args "$@" \
        --pid-file "$runner_pid_file" \
        --cancel-file "$cancel_file" \
        --identity-stability "$identity_stability" \
        --source-revision "$source_revision" \
        --source-worktree "$source_worktree" \
        --source-package-digest "$source_package_digest" &
    open_pid=$!
    (
        sleep "$runner_timeout_seconds"
        if kill -0 "$open_pid" 2>/dev/null; then
            : >"$timeout_marker"
            : >"$cancel_file"
            target_pid=
            if [ -s "$runner_pid_file" ]; then
                IFS= read -r target_pid <"$runner_pid_file" || target_pid=
            fi
            case $target_pid in
                ''|*[!0-9]*) target_pid= ;;
            esac
            if [ -n "$target_pid" ] && [ "$target_pid" -gt 1 ]; then
                kill -TERM -- "-$target_pid" 2>/dev/null || \
                    kill -TERM "$target_pid" 2>/dev/null || true
            fi
            kill -TERM "$open_pid" 2>/dev/null || true
            sleep 1
            if [ -z "$target_pid" ] && [ -s "$runner_pid_file" ]; then
                IFS= read -r target_pid <"$runner_pid_file" || target_pid=
                case $target_pid in
                    ''|*[!0-9]*) target_pid= ;;
                esac
            fi
            if [ -n "$target_pid" ] && [ "$target_pid" -gt 1 ]; then
                kill -KILL -- "-$target_pid" 2>/dev/null || \
                    kill -KILL "$target_pid" 2>/dev/null || true
            fi
            kill -KILL "$open_pid" 2>/dev/null || true
        fi
    ) &
    watchdog_pid=$!
    runner_status=0
    wait "$open_pid" || runner_status=$?
    kill "$watchdog_pid" 2>/dev/null || true
    wait "$watchdog_pid" 2>/dev/null || true
    if [ -f "$timeout_marker" ]; then
        runner_status=124
    fi
    if [ -s "$runner_pid_file" ]; then
        observed_pid=
        IFS= read -r observed_pid <"$runner_pid_file" || observed_pid=
        case $observed_pid in
            ''|*[!0-9]*) ;;
            *) runner_pid_observed=true ;;
        esac
    fi
    if [ "$runner_status" -eq 0 ] && [ ! -s "$runner_pid_file" ]; then
        runner_status=1
    fi
    if [ "$runner_status" -ne 124 ]; then
        rm -f "$cancel_file"
    else
        # Keep the cancellation tombstone briefly so a delayed LaunchServices
        # start exits before touching TCC or launching the fixture.
        (sleep 10; rm -f "$cancel_file") >/dev/null 2>&1 &
    fi
    rm -f "$timeout_marker" "$runner_pid_file"
    return "$runner_status"
}

write_launcher_report() {
    report_status=$1
    report_message=$2
    report_phase=$3
    report_error_code=$4
    report_command_status=$5
    report_timed_out=$6
    case $report_status in
        unsupported) report_check_status=unsupported; report_failed=0 ;;
        *) report_check_status=fail; report_failed=1 ;;
    esac
    report_os=$(uname -s 2>/dev/null || printf unknown)
    case $report_os in
        Darwin|Linux) ;;
        *) report_os=unknown ;;
    esac
    report_source=
    if [ "$source_verified" = true ]; then
        report_source=',"source":{"revision":"'"$source_revision"'","worktree":"'"$source_worktree"'","package_digest":"'"$source_package_digest"'"}'
    fi
    cat >"$report_path" <<EOF
{"schema_version":"1.0","kind":"macos_ax_fixture_test","status":"$report_status","message":"$report_message"$report_source,"platform":{"os":"$report_os","architecture":"$arch"},"permissions":{"accessibility":{"checked":false,"prompt_requested":$prompt_accessibility},"screen_capture":{"checked":false,"request_attempted":false,"capture_attempted":false}},"execution":{"phase":"$report_phase","command_status":$report_command_status,"timed_out":$report_timed_out,"timeout_seconds":$runner_timeout_seconds,"runner_pid_observed":$runner_pid_observed},"error":{"code":"$report_error_code","message":"$report_message"},"checks":[{"id":"launcher_$report_phase","status":"$report_check_status","message":"$report_message"}],"summary":{"passed":0,"failed":$report_failed,"total":1}}
EOF
}

build_status=0
if [ "${write_provenance_error:-}" = invalid_source_provenance ]; then
    build_status=82
else
    "$script_dir/build.sh" "$build_root" || build_status=$?
fi
if [ "$build_status" -ne 0 ]; then
    case $build_status in
        69) write_launcher_report unsupported "requires_macos" build requires_macos "$build_status" false; final_status=3 ;;
        70) write_launcher_report unsupported "xcode_command_line_tools_missing" build xcode_command_line_tools_missing "$build_status" false; final_status=3 ;;
        71) write_launcher_report unsupported "swift_toolchain_unavailable" build swift_toolchain_unavailable "$build_status" false; final_status=3 ;;
        74) write_launcher_report unsupported "codesign_unavailable" build codesign_unavailable "$build_status" false; final_status=3 ;;
        79) write_launcher_report unsupported "unsupported_architecture" build unsupported_architecture "$build_status" false; final_status=3 ;;
        81) write_launcher_report unsupported "unsupported_swift_version" build unsupported_swift_version "$build_status" false; final_status=3 ;;
        82) write_launcher_report failed "invalid_source_provenance" build invalid_source_provenance "$build_status" false; final_status=1 ;;
        72) write_launcher_report failed "fixture_compile_failed" build fixture_compile_failed "$build_status" false; final_status=1 ;;
        73) write_launcher_report failed "runner_compile_failed" build runner_compile_failed "$build_status" false; final_status=1 ;;
        75) write_launcher_report failed "fixture_codesign_failed" build fixture_codesign_failed "$build_status" false; final_status=1 ;;
        76) write_launcher_report failed "runner_codesign_failed" build runner_codesign_failed "$build_status" false; final_status=1 ;;
        77|78) write_launcher_report failed "bundle_identity_validation_failed" build bundle_identity_validation_failed "$build_status" false; final_status=1 ;;
        80) write_launcher_report failed "identity_attestation_failed" build identity_attestation_failed "$build_status" false; final_status=1 ;;
        *) write_launcher_report failed "native_build_failed" build native_build_failed "$build_status" false; final_status=1 ;;
    esac
else
    identity_available_this_run=true
    runner="$build_root/AiAutoDesktopAXRunner.app/Contents/MacOS/AiAutoDesktopAXRunner"
    fixture="$build_root/AiAutoDesktopAXFixture.app"
    temporary_report="$result_dir/report.json.tmp"
    runner_status=0
    if [ "${MACOS_TEST_CODESIGN_IDENTITY:--}" = - ]; then
        identity_stability=ephemeral
    else
        identity_stability=stable_identity_requested
    fi
    if [ "$prompt_accessibility" = true ]; then
        run_with_watchdog --fixture-app "$fixture" \
            --prompt-accessibility \
            --report "$temporary_report" || runner_status=$?
    else
        run_with_watchdog --fixture-app "$fixture" \
            --report "$temporary_report" || runner_status=$?
    fi
    if [ "$runner_status" -eq 124 ]; then
        rm -f "$temporary_report"
        write_launcher_report failed "runner_timeout" runner runner_timeout "$runner_status" true
        final_status=1
    elif [ ! -s "$temporary_report" ]; then
        write_launcher_report failed "runner_produced_no_json" runner runner_produced_no_json "$runner_status" false
        final_status=1
    elif [ ! -x /usr/bin/plutil ]; then
        write_launcher_report failed "json_validator_unavailable" runner json_validator_unavailable "$runner_status" false
        final_status=1
    elif ! /usr/bin/plutil -lint "$temporary_report" >/dev/null 2>&1; then
        write_launcher_report failed "runner_produced_invalid_json" runner runner_produced_invalid_json "$runner_status" false
        final_status=1
    else
        mv "$temporary_report" "$report_path"
        if report_status=$(/usr/bin/plutil -extract status raw -o - "$report_path" 2>/dev/null); then
            case $report_status in
                passed) final_status=0 ;;
                unsupported) final_status=3 ;;
                failed) final_status=1 ;;
                *)
                    write_launcher_report failed "runner_report_status_invalid" runner runner_report_status_invalid "$runner_status" false
                    final_status=1
                    ;;
            esac
        else
            write_launcher_report failed "runner_report_status_missing" runner runner_report_status_missing "$runner_status" false
            final_status=1
        fi
    fi
fi

cat >"$result_dir/README.txt" <<'EOF'
此归档可回传给测试请求方。它只包含结构化测试结果和本说明：
- 未保存截图或屏幕像素；
- 未枚举 fixture 进程之外的 Accessibility 树；
- 未包含构建日志、用户名、主机名或其他应用的窗口内容。
- source revision/package digest 来自已验证的源码包 manifest，但仍需请求方用可信预期值校验。
EOF
if [ "$identity_available_this_run" = true ] \
    && [ -f "$build_root/identity.txt" ]; then
    cp "$build_root/identity.txt" "$result_dir/identity.txt"
else
    if [ "$source_verified" = true ]; then
        {
            printf '%s\n' 'identity_attestation=unavailable'
            printf '%s\n' "source_revision=$source_revision"
            printf '%s\n' "source_worktree=$source_worktree"
            printf '%s\n' "source_package_digest=$source_package_digest"
        } >"$result_dir/identity.txt"
    else
        printf '%s\n' 'identity_attestation=unavailable' \
            >"$result_dir/identity.txt"
    fi
fi

if [ -x /usr/bin/shasum ]; then
    if ! (cd "$result_dir" \
        && /usr/bin/shasum -a 256 report.json README.txt identity.txt \
            >SHA256SUMS.tmp); then
        rm -f "$result_dir/SHA256SUMS.tmp"
        printf '%s\n' '失败：无法计算结果文件 SHA-256。' >&2
        write_launcher_report failed "result_hash_failed" archive result_hash_failed 1 false
        cat "$report_path"
        exit 1
    fi
    mv "$result_dir/SHA256SUMS.tmp" "$result_dir/SHA256SUMS"
else
    printf '%s\n' '失败：系统 shasum 不可用，拒绝生成无校验清单的归档。' >&2
    write_launcher_report failed "shasum_unavailable" archive shasum_unavailable 69 false
    cat "$report_path"
    exit 1
fi
if ! create_normalized_tar_gz "$archive_path" "$result_dir" \
    report.json README.txt identity.txt SHA256SUMS; then
    printf '%s\n' '失败：无法创建结果归档。' >&2
    rm -f "$result_dir/SHA256SUMS"
    write_launcher_report failed "result_archive_failed" archive result_archive_failed 1 false
    cat "$report_path"
    exit 1
fi
if ! archive_sha256=$(/usr/bin/shasum -a 256 "$archive_path" 2>/dev/null); then
    printf '%s\n' '失败：无法计算结果归档 SHA-256。' >&2
    rm -f "$archive_path"
    write_launcher_report failed "archive_hash_failed" archive archive_hash_failed 1 false
    cat "$report_path"
    exit 1
fi
cat "$report_path"
printf '%s\n' "结果归档：$archive_path" >&2
printf '%s\n' "归档 SHA-256：${archive_sha256%% *}" >&2
exit "$final_status"

