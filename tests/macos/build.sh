#!/bin/sh
set -eu

script_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd -P)
. "$script_dir/identity.sh"
. "$script_dir/source-provenance.sh"
build_root=${1:-"$script_dir/.build"}
signing_identity=${MACOS_TEST_CODESIGN_IDENTITY:--}
fixture_app="$build_root/AiAutoDesktopAXFixture.app"
runner_app="$build_root/AiAutoDesktopAXRunner.app"
signing_marker="$build_root/.codesign-identity"
source_digest_marker="$build_root/.source-package-digest"
attestation="$build_root/identity.txt"
compile_diagnostics="$build_root/compile-diagnostics.txt"
compile_log=
compile_status_file=
compile_overflow_file=
rm -f "$attestation"

cleanup_compile_log() {
    if [ -n "${compile_log:-}" ]; then
        rm -f "$compile_log" 2>/dev/null || :
    fi
    if [ -n "${compile_status_file:-}" ]; then
        rm -f "$compile_status_file" 2>/dev/null || :
    fi
    if [ -n "${compile_overflow_file:-}" ]; then
        rm -f "$compile_overflow_file" 2>/dev/null || :
    fi
}
trap cleanup_compile_log 0
trap 'cleanup_compile_log; exit 1' HUP INT TERM

if ! load_source_provenance "$script_dir" \
    "$script_dir/SOURCE_PACKAGE_FILES.txt"; then
    printf '%s\n' '失败：源码包 provenance 验证失败。' >&2
    exit 82
fi
release_source_provenance

if [ "$(uname -s 2>/dev/null || printf unknown)" != Darwin ]; then
    printf '%s\n' '不支持：此构建脚本必须在 macOS 上运行。' >&2
    exit 69
fi
if ! command -v xcrun >/dev/null 2>&1; then
    printf '%s\n' '不支持：缺少 xcrun，请先安装 Xcode Command Line Tools。' >&2
    exit 70
fi
if ! swiftc_path=$(xcrun --sdk macosx --find swiftc 2>/dev/null); then
    printf '%s\n' '不支持：找不到 macOS SDK 或 swiftc，请运行 xcode-select --install。' >&2
    exit 71
fi
if ! swift_version_output=$("$swiftc_path" --version 2>&1); then
    printf '%s\n' '不支持：无法读取 Swift 编译器版本。' >&2
    exit 81
fi
swift_version_numbers=$(
    printf '%s\n' "$swift_version_output" \
        | sed -n '1s/.*Swift version \([0-9][0-9]*\)\.\([0-9][0-9]*\).*/\1 \2/p'
)
set -- $swift_version_numbers
if [ "$#" -ne 2 ]; then
    printf '%s\n' '不支持：无法解析 Swift 编译器版本（至少需要 Swift 5.3）。' >&2
    exit 81
fi
swift_major=$1
swift_minor=$2
if [ "$swift_major" -lt 5 ] \
    || { [ "$swift_major" -eq 5 ] && [ "$swift_minor" -lt 3 ]; }; then
    printf '%s\n' '不支持：至少需要 Swift 5.3（Xcode 12 或更新版本）。' >&2
    exit 81
fi
if ! sdk_path=$(xcrun --sdk macosx --show-sdk-path 2>/dev/null); then
    printf '%s\n' '不支持：找不到 macOS SDK。' >&2
    exit 71
fi
arch=$(uname -m 2>/dev/null || printf unknown)
case $arch in
    arm64|x86_64) target="$arch-apple-macos11.0" ;;
    *)
        printf '%s\n' "不支持的 macOS 架构：$arch" >&2
        exit 79
        ;;
esac
if ! command -v codesign >/dev/null 2>&1; then
    printf '%s\n' '不支持：系统 codesign 不可用。' >&2
    exit 74
fi

mkdir -p "$build_root"
rm -f "$compile_diagnostics"

write_compile_diagnostics() {
    diagnostic_phase=$1
    diagnostic_status=$2
    diagnostic_input=$3
    diagnostic_capture_truncated=$4
    diagnostic_temporary=$compile_diagnostics.writing-$$
    rm -f "$diagnostic_temporary"
    if ! (
        umask 077
        {
            printf '%s\n' \
                'schema=ai-auto-desktop.macos-compile-diagnostics/v1' \
                "phase=$diagnostic_phase" \
                "command_status=$diagnostic_status" \
                "swift=$swift_major.$swift_minor" \
                "target=$target" \
                'sanitized=true' \
                'max_lines=120' \
                'max_output_bytes=12288' \
                'raw_capture_limit_bytes=131072' \
                "raw_capture_truncated=$diagnostic_capture_truncated" \
                'compiler_output_begin'
            LC_ALL=C awk \
                -v diagnostic_build_root="$build_root" \
                -v diagnostic_script_dir="$script_dir" \
                -v diagnostic_sdk_path="$sdk_path" \
                -v diagnostic_swiftc_path="$swiftc_path" '
function replace_literal(value, needle, replacement, position) {
    if (needle == "") {
        return value
    }
    while ((position = index(value, needle)) != 0) {
        value = substr(value, 1, position - 1) replacement \
            substr(value, position + length(needle))
    }
    return value
}
function scrub(value) {
    value = replace_literal(value, diagnostic_build_root "/", "<BUILD>:")
    value = replace_literal(value, diagnostic_script_dir "/", "<TESTKIT>:")
    value = replace_literal(value, diagnostic_sdk_path "/", "<SDK>:")
    value = replace_literal(value, diagnostic_build_root, "<BUILD>")
    value = replace_literal(value, diagnostic_script_dir, "<TESTKIT>")
    value = replace_literal(value, diagnostic_sdk_path, "<SDK>")
    value = replace_literal(value, diagnostic_swiftc_path, "<SWIFTC>")
    gsub(/\/[[:alnum:]_.~+@%=-][^[:space:]"<>:]*/, "<ABSOLUTE_PATH>", value)
    gsub(/[^\t -~]/, "?", value)
    return value
}
BEGIN {
    emitted_lines = 0
    emitted_bytes = 0
    max_lines = 120
    max_bytes = 12288
    max_line_bytes = 512
    truncation_marker = "[compiler output truncated]"
    reserved_bytes = length(truncation_marker) + 1
}
{
    value = scrub($0)
    if (length(value) > max_line_bytes) {
        value = substr(value, 1, max_line_bytes - 20) " [line truncated]"
    }
    required = length(value) + 1
    if (emitted_lines >= max_lines - 1 \
            || emitted_bytes + required > max_bytes - reserved_bytes) {
        truncated = 1
        next
    }
    print value
    emitted_lines++
    emitted_bytes += required
}
END {
    if (NR == 0) {
        print "[compiler emitted no output]"
    }
    if (truncated) {
        print truncation_marker
    }
}
' "$diagnostic_input"
            printf '%s\n' 'compiler_output_end'
        } >"$diagnostic_temporary"
    ); then
        rm -f "$diagnostic_temporary"
        return 1
    fi
    if ! chmod 600 "$diagnostic_temporary" \
        || ! mv "$diagnostic_temporary" "$compile_diagnostics"; then
        rm -f "$diagnostic_temporary"
        return 1
    fi
}

compile_swift() {
    compile_phase=$1
    compile_failure_status=$2
    shift 2
    compile_capture_truncated=false
    if ! compile_log=$(mktemp "$build_root/.swiftc-output.XXXXXX"); then
        printf '%s\n' '失败：无法创建私有 Swift 编译诊断文件。' >&2
        exit "$compile_failure_status"
    fi
    chmod 600 "$compile_log"
    # Keep the first 128 KiB, then drain the rest so the compiler cannot be
    # killed by SIGPIPE before its real exit status is written to the sidecar.
    compile_status_file=$compile_log.status
    compile_overflow_file=$compile_log.overflow
    if (
        set +e
        "$@" 2>&1
        printf '%s\n' "$?" >"$compile_status_file"
    ) | (
        LC_ALL=C head -c 131072 >"$compile_log"
        compile_overflow_bytes=$(LC_ALL=C wc -c | tr -d '[:space:]')
        printf '%s\n' "$compile_overflow_bytes" >"$compile_overflow_file"
    ); then
        :
    else
        rm -f "$compile_status_file"
        printf '%s\n' '失败：无法有界捕获 Swift 编译器输出。' >&2
        return 1
    fi
    if [ ! -f "$compile_overflow_file" ] \
        || [ -L "$compile_overflow_file" ]; then
        printf '%s\n' '失败：无法读取 Swift 编译器输出边界。' >&2
        return 1
    fi
    IFS= read -r compile_overflow_bytes <"$compile_overflow_file" \
        || compile_overflow_bytes=
    rm -f "$compile_overflow_file"
    compile_overflow_file=
    case $compile_overflow_bytes in
        ''|*[!0-9]*)
            printf '%s\n' '失败：无法检查 Swift 编译器输出大小。' >&2
            return 1
            ;;
        0) ;;
        *) compile_capture_truncated=true ;;
    esac
    if [ ! -f "$compile_status_file" ] || [ -L "$compile_status_file" ]; then
        printf '%s\n' '失败：无法读取 Swift 编译器状态。' >&2
        return 1
    fi
    IFS= read -r compile_status <"$compile_status_file" || compile_status=
    rm -f "$compile_status_file"
    compile_status_file=
    case $compile_status in
        0) ;;
        ''|*[!0-9]*)
            printf '%s\n' '失败：Swift 编译器状态无效。' >&2
            return 1
            ;;
    esac
    if [ "$compile_status" -eq 0 ]; then
        if [ -s "$compile_log" ]; then
            if write_compile_diagnostics \
                "$compile_phase" 0 "$compile_log" \
                "$compile_capture_truncated"; then
                cat "$compile_diagnostics" >&2
                rm -f "$compile_diagnostics"
            else
                printf '%s\n' \
                    '警告：Swift 编译器有输出，但无法安全整理诊断。' >&2
            fi
        fi
        rm -f "$compile_log"
        compile_log=
        return 0
    fi
    if ! write_compile_diagnostics \
        "$compile_phase" "$compile_status" "$compile_log" \
        "$compile_capture_truncated"; then
        {
            printf '%s\n' \
                'schema=ai-auto-desktop.macos-compile-diagnostics/v1' \
                "phase=$compile_phase" \
                "command_status=$compile_status" \
                'sanitized=true' \
                "raw_capture_truncated=$compile_capture_truncated" \
                'diagnostic_unavailable=true'
        } >"$compile_diagnostics"
        chmod 600 "$compile_diagnostics"
    fi
    cat "$compile_diagnostics" >&2
    rm -f "$compile_log"
    compile_log=
    return 1
}

write_fixture_plist() {
    destination=$1
    cat >"$destination" <<'PLIST'
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
<key>CFBundleExecutable</key><string>AiAutoDesktopAXFixture</string>
<key>CFBundleIdentifier</key><string>dev.ai-auto-desktop.testkit.fixture</string>
<key>CFBundleInfoDictionaryVersion</key><string>6.0</string>
<key>CFBundleName</key><string>AI Auto Desktop AX Fixture</string>
<key>CFBundlePackageType</key><string>APPL</string>
<key>CFBundleShortVersionString</key><string>1.0</string>
<key>CFBundleVersion</key><string>1</string>
<key>LSMinimumSystemVersion</key><string>11.0</string>
<key>NSHighResolutionCapable</key><true/>
</dict></plist>
PLIST
}

write_runner_plist() {
    destination=$1
    cat >"$destination" <<'PLIST'
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
<key>CFBundleExecutable</key><string>AiAutoDesktopAXRunner</string>
<key>CFBundleIdentifier</key><string>dev.ai-auto-desktop.testkit.ax-runner</string>
<key>CFBundleInfoDictionaryVersion</key><string>6.0</string>
<key>CFBundleName</key><string>AI Auto Desktop AX Runner</string>
<key>CFBundlePackageType</key><string>APPL</string>
<key>CFBundleShortVersionString</key><string>1.0</string>
<key>CFBundleVersion</key><string>1</string>
<key>LSMinimumSystemVersion</key><string>11.0</string>
<key>LSUIElement</key><true/>
</dict></plist>
PLIST
}

build_fixture() {
    rm -rf "$fixture_app"
    mkdir -p "$fixture_app/Contents/MacOS"
    write_fixture_plist "$fixture_app/Contents/Info.plist"
    if ! compile_swift fixture 72 \
        "$swiftc_path" -O -sdk "$sdk_path" -target "$target" -framework AppKit \
        "$script_dir/FixtureApp.swift" \
        -o "$fixture_app/Contents/MacOS/AiAutoDesktopAXFixture"; then
        printf '%s\n' '失败：AppKit fixture 编译失败。' >&2
        exit 72
    fi
    if ! codesign --force --sign "$signing_identity" --timestamp=none "$fixture_app" >/dev/null 2>&1; then
        printf '%s\n' '失败：AppKit fixture ad-hoc 签名失败。' >&2
        exit 75
    fi
}

build_runner() {
    rm -rf "$runner_app"
    mkdir -p "$runner_app/Contents/MacOS"
    write_runner_plist "$runner_app/Contents/Info.plist"
    if ! compile_swift runner 73 \
        "$swiftc_path" -O -sdk "$sdk_path" -target "$target" \
        -framework AppKit -framework ApplicationServices -framework Carbon \
        -framework CoreGraphics \
        "$script_dir/AXTestRunner.swift" \
        -o "$runner_app/Contents/MacOS/AiAutoDesktopAXRunner"; then
        printf '%s\n' '失败：AX runner 编译失败。' >&2
        exit 73
    fi
    if ! codesign --force --sign "$signing_identity" --timestamp=none "$runner_app" >/dev/null 2>&1; then
        printf '%s\n' '失败：AX runner ad-hoc 签名失败。' >&2
        exit 76
    fi
}

previous_signing_identity=
previous_source_package_digest=
if [ -f "$signing_marker" ]; then
    previous_signing_identity=$(sed -n '1p' "$signing_marker")
fi
if [ -f "$source_digest_marker" ]; then
    previous_source_package_digest=$(sed -n '1p' "$source_digest_marker")
fi
if [ ! -x "$fixture_app/Contents/MacOS/AiAutoDesktopAXFixture" ] \
    || [ "$script_dir/FixtureApp.swift" -nt "$fixture_app/Contents/MacOS/AiAutoDesktopAXFixture" ] \
    || [ "$0" -nt "$fixture_app/Contents/MacOS/AiAutoDesktopAXFixture" ] \
    || [ "$previous_source_package_digest" != "$SOURCE_PACKAGE_DIGEST" ] \
    || [ "$previous_signing_identity" != "$signing_identity" ]; then
    build_fixture
fi
if [ ! -x "$runner_app/Contents/MacOS/AiAutoDesktopAXRunner" ] \
    || [ "$script_dir/AXTestRunner.swift" -nt "$runner_app/Contents/MacOS/AiAutoDesktopAXRunner" ] \
    || [ "$0" -nt "$runner_app/Contents/MacOS/AiAutoDesktopAXRunner" ] \
    || [ "$previous_source_package_digest" != "$SOURCE_PACKAGE_DIGEST" ] \
    || [ "$previous_signing_identity" != "$signing_identity" ]; then
    build_runner
fi
printf '%s\n' "$signing_identity" >"$signing_marker"
printf '%s\n' "$SOURCE_PACKAGE_DIGEST" >"$source_digest_marker"

if [ "$signing_identity" = - ]; then
    identity_stability=ephemeral
else
    identity_stability=stable_identity_requested
fi
for app in "$fixture_app" "$runner_app"; do
    if ! codesign --verify --strict "$app" >/dev/null 2>&1; then
        printf '%s\n' "失败：签名严格校验失败：$app" >&2
        exit 77
    fi
done
if [ ! -x /usr/libexec/PlistBuddy ]; then
    printf '%s\n' '失败：系统 PlistBuddy 不可用。' >&2
    exit 77
fi
if [ "$(/usr/libexec/PlistBuddy -c 'Print :CFBundleIdentifier' "$fixture_app/Contents/Info.plist")" != "dev.ai-auto-desktop.testkit.fixture" ] \
    || [ "$(/usr/libexec/PlistBuddy -c 'Print :CFBundleIdentifier' "$runner_app/Contents/Info.plist")" != "dev.ai-auto-desktop.testkit.ax-runner" ]; then
    printf '%s\n' '失败：构建产物 bundle ID 不符合预期。' >&2
    exit 78
fi
if ! write_identity_attestation \
    "$attestation" \
    "$swiftc_path" \
    "$(command -v codesign)" \
    /usr/bin/lipo \
    /usr/bin/shasum \
    "$identity_stability" \
    "$SOURCE_REVISION" \
    "$SOURCE_WORKTREE" \
    "$SOURCE_PACKAGE_DIGEST" \
    "$runner_app" \
    "$runner_app/Contents/MacOS/AiAutoDesktopAXRunner" \
    dev.ai-auto-desktop.testkit.ax-runner \
    "$fixture_app" \
    "$fixture_app/Contents/MacOS/AiAutoDesktopAXFixture" \
    dev.ai-auto-desktop.testkit.fixture; then
    rm -f "$attestation"
    printf '%s\n' '失败：无法生成完整的 identity attestation。' >&2
    exit 80
fi

printf '%s\n' "构建完成：$build_root" >&2

