#!/bin/sh
set -eu

script_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd -P)
. "$script_dir/identity.sh"
build_root=${1:-"$script_dir/.build"}
signing_identity=${MACOS_TEST_CODESIGN_IDENTITY:--}
fixture_app="$build_root/AiAutoDesktopAXFixture.app"
runner_app="$build_root/AiAutoDesktopAXRunner.app"
signing_marker="$build_root/.codesign-identity"
attestation="$build_root/identity.txt"
rm -f "$attestation"

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
    if ! "$swiftc_path" -O -sdk "$sdk_path" -target "$target" -framework AppKit \
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
    if ! "$swiftc_path" -O -sdk "$sdk_path" -target "$target" \
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
if [ -f "$signing_marker" ]; then
    previous_signing_identity=$(sed -n '1p' "$signing_marker")
fi
if [ ! -x "$fixture_app/Contents/MacOS/AiAutoDesktopAXFixture" ] \
    || [ "$script_dir/FixtureApp.swift" -nt "$fixture_app/Contents/MacOS/AiAutoDesktopAXFixture" ] \
    || [ "$0" -nt "$fixture_app/Contents/MacOS/AiAutoDesktopAXFixture" ] \
    || [ "$previous_signing_identity" != "$signing_identity" ]; then
    build_fixture
fi
if [ ! -x "$runner_app/Contents/MacOS/AiAutoDesktopAXRunner" ] \
    || [ "$script_dir/AXTestRunner.swift" -nt "$runner_app/Contents/MacOS/AiAutoDesktopAXRunner" ] \
    || [ "$0" -nt "$runner_app/Contents/MacOS/AiAutoDesktopAXRunner" ] \
    || [ "$previous_signing_identity" != "$signing_identity" ]; then
    build_runner
fi
printf '%s\n' "$signing_identity" >"$signing_marker"

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

