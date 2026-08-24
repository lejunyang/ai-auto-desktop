#!/bin/sh
set -eu

script_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd -P)
build_root="$script_dir/.build"
output_root="$script_dir/results"
prompt_accessibility=false
runner_timeout_seconds=30
runner_pid=
watchdog_pid=

usage() {
    cat >&2 <<'EOF'
用法：tests/macos/run.sh [--prompt-accessibility] [--output DIR] [--build-dir DIR]

默认只检查 Accessibility trust，不弹授权提示。只有显式传入
--prompt-accessibility 才会调用带 prompt 的系统 API。套件绝不请求截屏授权，也不截图。
EOF
}

while [ "$#" -gt 0 ]; do
    case $1 in
        --prompt-accessibility)
            prompt_accessibility=true
            shift
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
if [ -e "$result_dir" ] || [ -L "$result_dir" ]; then
    printf '%s\n' '失败：结果目录已存在。' >&2
    exit 1
fi
mkdir "$result_dir"
chmod 700 "$result_dir"

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
        --identity-stability "$identity_stability" &
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

write_environment_report() {
    report_status=$1
    report_message=$2
    cat >"$report_path" <<EOF
{"schema_version":"1.0","kind":"macos_ax_fixture_test","status":"$report_status","message":"$report_message","platform":{"os":"$(uname -s 2>/dev/null || printf unknown)","architecture":"$arch"},"permissions":{"accessibility":{"checked":false,"prompt_requested":$prompt_accessibility},"screen_capture":{"checked":false,"request_attempted":false,"capture_attempted":false}},"checks":[],"summary":{"passed":0,"failed":0,"total":0}}
EOF
}

build_status=0
"$script_dir/build.sh" "$build_root" || build_status=$?
if [ "$build_status" -ne 0 ]; then
    case $build_status in
        69) write_environment_report unsupported "requires_macos"; final_status=3 ;;
        70|71|74|79) write_environment_report unsupported "xcode_command_line_tools_or_architecture_unavailable"; final_status=3 ;;
        *) write_environment_report failed "native_build_failed"; final_status=1 ;;
    esac
else
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
    if [ ! -s "$temporary_report" ]; then
        write_environment_report failed "runner_produced_no_json"
        final_status=1
    elif command -v plutil >/dev/null 2>&1 && ! plutil -lint "$temporary_report" >/dev/null 2>&1; then
        write_environment_report failed "runner_produced_invalid_json"
        final_status=1
    else
        mv "$temporary_report" "$report_path"
        report_status=$(/usr/bin/plutil -extract status raw -o - "$report_path" 2>/dev/null || printf invalid)
        case $report_status in
            passed) final_status=0 ;;
            unsupported) final_status=3 ;;
            *) final_status=1 ;;
        esac
    fi
fi

cat >"$result_dir/README.txt" <<'EOF'
此归档可回传给测试请求方。它只包含结构化测试结果和本说明：
- 未保存截图或屏幕像素；
- 未枚举 fixture 进程之外的 Accessibility 树；
- 未包含构建日志、用户名、主机名或其他应用的窗口内容。
EOF
if [ -f "$build_root/identity.txt" ]; then
    cp "$build_root/identity.txt" "$result_dir/identity.txt"
else
    printf '%s\n' 'identity_attestation=unavailable' >"$result_dir/identity.txt"
fi

if command -v shasum >/dev/null 2>&1; then
    (cd "$result_dir" && shasum -a 256 report.json README.txt identity.txt) \
        >"$result_dir/SHA256SUMS"
else
    printf '%s\n' 'sha256_manifest=unavailable' >"$result_dir/SHA256SUMS"
fi
if ! tar -czf "$archive_path" -C "$result_dir" \
    report.json README.txt identity.txt SHA256SUMS; then
    printf '%s\n' '失败：无法创建结果归档。' >&2
    cat "$report_path"
    exit 1
fi
cat "$report_path"
printf '%s\n' "结果归档：$archive_path" >&2
exit "$final_status"

