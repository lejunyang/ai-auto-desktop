#!/bin/sh
set -eu

script_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
build_root=${AI_AUTO_DESKTOP_MACOS_AX_BUILD_DIR:-"$script_dir/.build"}
app_bundle="$build_root/MacOSAXHelper.app"
contents="$app_bundle/Contents"
executable="$contents/MacOS/MacOSAXHelper"
identity=${MACOS_AX_CODESIGN_IDENTITY:--}

if [ "$(uname -s)" != "Darwin" ]; then
    echo "macOS AX helper 只能在 macOS 构建" >&2
    exit 3
fi
if ! command -v xcrun >/dev/null 2>&1; then
    echo "缺少 Xcode Command Line Tools（xcrun）" >&2
    exit 3
fi
if ! swiftc_path=$(xcrun --sdk macosx --find swiftc 2>/dev/null); then
    echo "缺少 macOS SDK 或 swiftc" >&2
    exit 3
fi
if ! sdk_path=$(xcrun --sdk macosx --show-sdk-path 2>/dev/null); then
    echo "缺少 macOS SDK" >&2
    exit 3
fi
arch=$(uname -m)
case $arch in
    arm64|x86_64) target="$arch-apple-macos11.0" ;;
    *)
        echo "不支持的 macOS 架构：$arch" >&2
        exit 3
        ;;
esac
if [ ! -x /usr/bin/codesign ]; then
    echo "缺少系统 codesign" >&2
    exit 3
fi

rm -rf "$app_bundle"
mkdir -p "$contents/MacOS"
"$swiftc_path" \
    -O \
    -sdk "$sdk_path" \
    -target "$target" \
    -framework AppKit \
    -framework ApplicationServices \
    -framework Carbon \
    "$script_dir/swift/MacOSAXHelper.swift" \
    -o "$executable"

info_plist="$contents/Info.plist"
{
    echo '<?xml version="1.0" encoding="UTF-8"?>'
    echo '<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">'
    echo '<plist version="1.0"><dict>'
    echo '<key>CFBundleExecutable</key><string>MacOSAXHelper</string>'
    echo '<key>CFBundleIdentifier</key><string>dev.ai-auto-desktop.macos-ax-helper</string>'
    echo '<key>CFBundleInfoDictionaryVersion</key><string>6.0</string>'
    echo '<key>CFBundleName</key><string>MacOSAXHelper</string>'
    echo '<key>CFBundlePackageType</key><string>APPL</string>'
    echo '<key>CFBundleShortVersionString</key><string>0.1.0</string>'
    echo '<key>CFBundleVersion</key><string>1</string>'
    echo '<key>LSBackgroundOnly</key><true/>'
    echo '<key>NSAppleEventsUsageDescription</key><string>Not used; this helper only calls macOS Accessibility APIs.</string>'
    echo '</dict></plist>'
} >"$info_plist"

/usr/bin/codesign --force --sign "$identity" --timestamp=none --identifier dev.ai-auto-desktop.macos-ax-helper "$app_bundle"
/usr/bin/codesign --verify --strict --deep --verbose=2 "$app_bundle"

if [ ! -x /usr/libexec/PlistBuddy ]; then
    echo "缺少系统 PlistBuddy" >&2
    exit 3
fi
if [ "$(/usr/libexec/PlistBuddy -c 'Print :CFBundleIdentifier' "$info_plist")" != dev.ai-auto-desktop.macos-ax-helper ] \
    || [ "$(/usr/libexec/PlistBuddy -c 'Print :CFBundleExecutable' "$info_plist")" != MacOSAXHelper ]; then
    echo "构建产物 bundle identity 不符合预期" >&2
    exit 3
fi

echo "已构建并签名：$app_bundle"
echo "helper：$executable"
