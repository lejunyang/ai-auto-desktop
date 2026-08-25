import AppKit
import ApplicationServices
import Carbon.HIToolbox
import CoreGraphics
import Darwin
import Foundation

private let fixtureBundleID = "dev.ai-auto-desktop.testkit.fixture"
private let runnerBundleID = "dev.ai-auto-desktop.testkit.ax-runner"
private let testValue = "AX updated value"
private let typeTextValue = "ASCII 中文 😀"
private let expectedStatus = "Status: pressed: \(typeTextValue)"
private let expectedInitialPointerStatus = "Pointer status: idle"
private let expectedPointerStatus = "Pointer status: clicked"
private let maximumUnicodeUnitsPerEvent = 20
private let axMessageTimeout: Float = 1.0
private var reportIdentityStability = "unknown"
private var reportSourceRevision = "unavailable"
private var reportSourceWorktree = "unavailable"
private var reportSourcePackageDigest = "unavailable"

private struct Arguments {
    let fixtureApp: URL
    let reportPath: URL?
    let pidFile: URL?
    let cancelFile: URL?
    let identityStability: String
    let sourceRevision: String
    let sourceWorktree: String
    let sourcePackageDigest: String
    let promptAccessibility: Bool
    let maxDepth: Int
    let maxNodes: Int
}

private struct Node {
    let element: AXUIElement
    let role: String?
    let subrole: String?
    let title: String?
    let identifier: String?
    let value: String?
    let focused: Bool
}

private struct Snapshot {
    let nodes: [Node]
    let truncated: Bool
    let axErrors: [Int]
}

private struct Outcome {
    let report: [String: Any]
    let exitCode: Int32
}

private func architectureName() -> String {
#if arch(arm64)
    return "arm64"
#elseif arch(x86_64)
    return "x86_64"
#else
    return "unknown"
#endif
}

private func isLowercaseHex(_ value: String, lengths: Set<Int>) -> Bool {
    let bytes = Array(value.utf8)
    guard lengths.contains(bytes.count) else { return false }
    return bytes.allSatisfy { byte in
        (48...57).contains(byte) || (97...102).contains(byte)
    }
}

// Give the shell watchdog a dedicated process group; the fixture inherits it.
// If the runner hangs, the launcher can terminate the complete test process tree.
private func configureProcessGroup() -> Bool {
    if getpgrp() == getpid() { return true }
    return setpgid(0, 0) == 0
}

private func writePIDFile(_ destination: URL?) -> Bool {
    guard let destination = destination else { return true }
    let data = Data("\(getpid())\n".utf8)
    do {
        try data.write(to: destination, options: .atomic)
        return true
    } catch {
        return false
    }
}

private func terminateAndReap(_ pid: pid_t) {
    guard pid > 1 else { return }
    var status: Int32 = 0
    _ = kill(pid, SIGTERM)
    let termDeadline = Date().addingTimeInterval(2)
    var reaped = false
    while Date() < termDeadline {
        if waitpid(pid, &status, WNOHANG) == pid {
            reaped = true
            break
        }
        Thread.sleep(forTimeInterval: 0.05)
    }
    if !reaped {
        _ = kill(pid, SIGKILL)
        let killDeadline = Date().addingTimeInterval(2)
        while Date() < killDeadline {
            if waitpid(pid, &status, WNOHANG) == pid { break }
            Thread.sleep(forTimeInterval: 0.05)
        }
    }
}

private func rosettaTranslated() -> Bool {
    var value: Int32 = 0
    var size = MemoryLayout<Int32>.size
    return sysctlbyname("sysctl.proc_translated", &value, &size, nil, 0) == 0
        && value == 1
}

private func settable(_ element: AXUIElement, _ name: CFString) -> (AXError, Bool) {
    let timeoutError = AXUIElementSetMessagingTimeout(element, axMessageTimeout)
    guard timeoutError == .success else { return (timeoutError, false) }
    var result = DarwinBoolean(false)
    let error = AXUIElementIsAttributeSettable(element, name, &result)
    return (error, result.boolValue)
}

private func actions(_ element: AXUIElement) -> (AXError, [String]) {
    let timeoutError = AXUIElementSetMessagingTimeout(element, axMessageTimeout)
    guard timeoutError == .success else { return (timeoutError, []) }
    var raw: CFArray?
    let error = AXUIElementCopyActionNames(element, &raw)
    guard error == .success, let values = raw else { return (error, []) }
    var result: [String] = []
    for index in 0..<CFArrayGetCount(values) {
        let pointer = CFArrayGetValueAtIndex(values, index)
        let value = Unmanaged<CFTypeRef>.fromOpaque(pointer).takeUnretainedValue()
        guard CFGetTypeID(value) == CFStringGetTypeID() else {
            return (.illegalArgument, [])
        }
        result.append(value as! String)
    }
    return (.success, result)
}

private struct TypeTextDispatch {
    let submitted: Bool
    let utf16UnitsPosted: Int
}

private struct PointerClickDispatch {
    let submitted: Bool
    let frontmostAtDispatch: Bool
}

private func postLeftClick(at point: CGPoint, to pid: pid_t) -> PointerClickDispatch {
    guard let move = CGEvent(
        mouseEventSource: nil, mouseType: .mouseMoved,
        mouseCursorPosition: point, mouseButton: .left
    ), let down = CGEvent(
        mouseEventSource: nil, mouseType: .leftMouseDown,
        mouseCursorPosition: point, mouseButton: .left
    ), let up = CGEvent(
        mouseEventSource: nil, mouseType: .leftMouseUp,
        mouseCursorPosition: point, mouseButton: .left
    ) else {
        return PointerClickDispatch(
            submitted: false, frontmostAtDispatch: false
        )
    }
    move.setIntegerValueField(.mouseEventClickState, value: 1)
    down.setIntegerValueField(.mouseEventClickState, value: 1)
    up.setIntegerValueField(.mouseEventClickState, value: 1)
    let frontmostAtDispatch =
        NSWorkspace.shared.frontmostApplication?.processIdentifier == pid
    guard frontmostAtDispatch else {
        return PointerClickDispatch(
            submitted: false, frontmostAtDispatch: false
        )
    }
    move.postToPid(pid)
    down.postToPid(pid)
    up.postToPid(pid)
    return PointerClickDispatch(
        submitted: true, frontmostAtDispatch: true
    )
}

private func typeTextTargetIsEligible(_ node: Node) -> Bool {
    let protected = node.role == "AXSecureTextField" || node.subrole == "AXSecureTextField"
    return !protected && ["AXTextField", "AXTextArea", "AXComboBox"].contains(node.role ?? "")
}

private func postUnicodeText(_ text: String, to pid: pid_t) -> TypeTextDispatch {
    let utf16 = Array(text.utf16)
    guard !utf16.isEmpty else {
        return TypeTextDispatch(submitted: false, utf16UnitsPosted: 0)
    }
    var offset = 0
    while offset < utf16.count {
        var end = min(offset + maximumUnicodeUnitsPerEvent, utf16.count)
        if end < utf16.count, utf16[end - 1] >= 0xD800, utf16[end - 1] <= 0xDBFF {
            end -= 1
        }
        guard end > offset,
              let keyDown = CGEvent(keyboardEventSource: nil, virtualKey: 0, keyDown: true),
              let keyUp = CGEvent(keyboardEventSource: nil, virtualKey: 0, keyDown: false) else {
            return TypeTextDispatch(submitted: false, utf16UnitsPosted: offset)
        }
        let chunk = Array(utf16[offset..<end])
        chunk.withUnsafeBufferPointer { buffer in
            keyDown.keyboardSetUnicodeString(
                stringLength: buffer.count, unicodeString: buffer.baseAddress
            )
            keyUp.keyboardSetUnicodeString(
                stringLength: buffer.count, unicodeString: buffer.baseAddress
            )
        }
        keyDown.postToPid(pid)
        keyUp.postToPid(pid)
        offset = end
    }
    return TypeTextDispatch(submitted: true, utf16UnitsPosted: offset)
}

private func attribute<T>(
    _ element: AXUIElement,
    _ name: CFString,
    as type: T.Type,
    errors: inout [Int]
) -> T? {
    let timeoutError = AXUIElementSetMessagingTimeout(element, axMessageTimeout)
    guard timeoutError == .success else {
        errors.append(Int(timeoutError.rawValue))
        return nil
    }
    var raw: CFTypeRef?
    let copyError = AXUIElementCopyAttributeValue(element, name, &raw)
    guard copyError == .success else {
        if copyError != .attributeUnsupported && copyError != .noValue {
            errors.append(Int(copyError.rawValue))
        }
        return nil
    }
    return raw as? T
}

private func elementBounds(
    _ element: AXUIElement, errors: inout [Int]
) -> CGRect? {
    let timeoutError = AXUIElementSetMessagingTimeout(element, axMessageTimeout)
    guard timeoutError == .success else {
        errors.append(Int(timeoutError.rawValue))
        return nil
    }
    var rawPosition: CFTypeRef?
    let positionError = AXUIElementCopyAttributeValue(
        element, kAXPositionAttribute as CFString, &rawPosition
    )
    guard positionError == .success, let positionReference = rawPosition,
          CFGetTypeID(positionReference) == AXValueGetTypeID() else {
        errors.append(Int(positionError == .success
            ? AXError.illegalArgument.rawValue : positionError.rawValue))
        return nil
    }
    var rawSize: CFTypeRef?
    let sizeError = AXUIElementCopyAttributeValue(
        element, kAXSizeAttribute as CFString, &rawSize
    )
    guard sizeError == .success, let sizeReference = rawSize,
          CFGetTypeID(sizeReference) == AXValueGetTypeID() else {
        errors.append(Int(sizeError == .success
            ? AXError.illegalArgument.rawValue : sizeError.rawValue))
        return nil
    }
    let positionValue = unsafeBitCast(positionReference, to: AXValue.self)
    let sizeValue = unsafeBitCast(sizeReference, to: AXValue.self)
    guard AXValueGetType(positionValue) == .cgPoint,
          AXValueGetType(sizeValue) == .cgSize else {
        errors.append(Int(AXError.illegalArgument.rawValue))
        return nil
    }
    var position = CGPoint.zero
    var size = CGSize.zero
    guard AXValueGetValue(positionValue, .cgPoint, &position),
          AXValueGetValue(sizeValue, .cgSize, &size),
          position.x.isFinite, position.y.isFinite,
          size.width.isFinite, size.height.isFinite else {
        errors.append(Int(AXError.illegalArgument.rawValue))
        return nil
    }
    return CGRect(origin: position, size: size)
}

private func boundedChildren(
    _ element: AXUIElement, remaining: Int, errors: inout [Int]
) -> (children: [AXUIElement], truncated: Bool) {
    let timeoutError = AXUIElementSetMessagingTimeout(element, axMessageTimeout)
    guard timeoutError == .success else {
        errors.append(Int(timeoutError.rawValue))
        return ([], true)
    }
    var count: CFIndex = 0
    let countError = AXUIElementGetAttributeValueCount(
        element, kAXChildrenAttribute as CFString, &count
    )
    if countError == .attributeUnsupported || countError == .noValue {
        return ([], false)
    }
    guard countError == .success, count >= 0 else {
        errors.append(Int(countError.rawValue))
        return ([], true)
    }
    let requested = min(Int(count), max(0, remaining))
    guard requested > 0 else { return ([], count > 0) }
    var rawValues: CFArray?
    let valuesError = AXUIElementCopyAttributeValues(
        element, kAXChildrenAttribute as CFString, 0, requested, &rawValues
    )
    guard valuesError == .success, let values = rawValues else {
        errors.append(Int(valuesError.rawValue))
        return ([], true)
    }
    var children: [AXUIElement] = []
    for index in 0..<CFArrayGetCount(values) {
        let pointer = CFArrayGetValueAtIndex(values, index)
        let value = Unmanaged<CFTypeRef>.fromOpaque(pointer).takeUnretainedValue()
        guard CFGetTypeID(value) == AXUIElementGetTypeID() else {
            errors.append(Int(AXError.illegalArgument.rawValue))
            return ([], true)
        }
        children.append(unsafeBitCast(value, to: AXUIElement.self))
    }
    return (children, Int(count) > requested)
}

private func snapshot(_ root: AXUIElement, maxDepth: Int, maxNodes: Int) -> Snapshot {
    var queue: [(AXUIElement, Int)] = [(root, 0)]
    var cursor = 0
    var nodes: [Node] = []
    var truncated = false
    var axErrors: [Int] = []
    var visited: [AXUIElement] = []

    while cursor < queue.count && nodes.count < maxNodes {
        let (element, depth) = queue[cursor]
        cursor += 1
        if visited.contains(where: { CFEqual($0, element) }) {
            continue
        }
        visited.append(element)
        let role: String? = attribute(
            element, kAXRoleAttribute as CFString, as: String.self, errors: &axErrors
        )
        let subrole: String? = attribute(
            element, kAXSubroleAttribute as CFString, as: String.self, errors: &axErrors
        )
        let protected = role == "AXSecureTextField" || subrole == "AXSecureTextField"
        nodes.append(
            Node(
                element: element,
                role: role,
                subrole: subrole,
                title: attribute(element, kAXTitleAttribute as CFString, as: String.self, errors: &axErrors),
                identifier: attribute(element, kAXIdentifierAttribute as CFString, as: String.self, errors: &axErrors),
                value: protected ? nil : attribute(
                    element, kAXValueAttribute as CFString, as: String.self, errors: &axErrors
                ),
                focused: attribute(element, kAXFocusedAttribute as CFString, as: Bool.self, errors: &axErrors) ?? false
            )
        )

        let remaining = maxNodes - nodes.count - (queue.count - cursor)
        let childResult = boundedChildren(
            element, remaining: remaining, errors: &axErrors
        )
        let children = childResult.children
        if childResult.truncated { truncated = true }
        if depth < maxDepth {
            queue.append(contentsOf: children.map { ($0, depth + 1) })
        } else if !children.isEmpty {
            truncated = true
        }
    }
    if cursor < queue.count {
        truncated = true
    }
    return Snapshot(nodes: nodes, truncated: truncated, axErrors: axErrors)
}

private func parseArguments() -> Arguments? {
    var fixturePath: String?
    var reportPath: String?
    var pidFile: String?
    var cancelFile: String?
    var identityStability = "unknown"
    var sourceRevision: String?
    var sourceWorktree: String?
    var sourcePackageDigest: String?
    var prompt = false
    var maxDepth = 8
    var maxNodes = 128
    var index = 1
    let raw = CommandLine.arguments

    while index < raw.count {
        switch raw[index] {
        case "--fixture-app":
            guard index + 1 < raw.count else { return nil }
            fixturePath = raw[index + 1]
            index += 2
        case "--prompt-accessibility":
            prompt = true
            index += 1
        case "--report":
            guard index + 1 < raw.count else { return nil }
            reportPath = raw[index + 1]
            index += 2
        case "--pid-file":
            guard index + 1 < raw.count else { return nil }
            pidFile = raw[index + 1]
            index += 2
        case "--cancel-file":
            guard index + 1 < raw.count else { return nil }
            cancelFile = raw[index + 1]
            index += 2
        case "--identity-stability":
            guard index + 1 < raw.count,
                  ["ephemeral", "stable_identity_requested"].contains(raw[index + 1])
            else { return nil }
            identityStability = raw[index + 1]
            index += 2
        case "--source-revision":
            guard index + 1 < raw.count,
                  isLowercaseHex(raw[index + 1], lengths: [40, 64])
            else { return nil }
            sourceRevision = raw[index + 1]
            index += 2
        case "--source-worktree":
            guard index + 1 < raw.count,
                  ["clean", "dirty"].contains(raw[index + 1])
            else { return nil }
            sourceWorktree = raw[index + 1]
            index += 2
        case "--source-package-digest":
            guard index + 1 < raw.count,
                  isLowercaseHex(raw[index + 1], lengths: [64])
            else { return nil }
            sourcePackageDigest = raw[index + 1]
            index += 2
        case "--max-depth":
            guard index + 1 < raw.count, let value = Int(raw[index + 1]), value > 0 else { return nil }
            maxDepth = min(value, 32)
            index += 2
        case "--max-nodes":
            guard index + 1 < raw.count, let value = Int(raw[index + 1]), value > 0 else { return nil }
            maxNodes = min(value, 2_048)
            index += 2
        default:
            return nil
        }
    }
    guard let fixturePath = fixturePath,
          let sourceRevision = sourceRevision,
          let sourceWorktree = sourceWorktree,
          let sourcePackageDigest = sourcePackageDigest else { return nil }
    return Arguments(
        fixtureApp: URL(fileURLWithPath: fixturePath).standardizedFileURL,
        reportPath: reportPath.map {
            URL(fileURLWithPath: $0).standardizedFileURL
        },
        pidFile: pidFile.map {
            URL(fileURLWithPath: $0).standardizedFileURL
        },
        cancelFile: cancelFile.map {
            URL(fileURLWithPath: $0).standardizedFileURL
        },
        identityStability: identityStability,
        sourceRevision: sourceRevision,
        sourceWorktree: sourceWorktree,
        sourcePackageDigest: sourcePackageDigest,
        promptAccessibility: prompt,
        maxDepth: maxDepth,
        maxNodes: maxNodes
    )
}

private func makeReport(
    status: String,
    message: String,
    promptRequested: Bool,
    accessibilityTrusted: Bool,
    screenCaptureGranted: Bool,
    checks: [[String: Any]],
    identityStability: String = reportIdentityStability
) -> [String: Any] {
    let passed = checks.filter { ($0["status"] as? String) == "pass" }.count
    let failed = checks.filter { ($0["status"] as? String) == "fail" }.count
    let actualRunnerBundleID = Bundle.main.bundleIdentifier
    return [
        "schema_version": "1.0",
        "kind": "macos_ax_fixture_test",
        "status": status,
        "message": message,
        "source": [
            "revision": reportSourceRevision,
            "worktree": reportSourceWorktree,
            "package_digest": reportSourcePackageDigest,
        ],
        "timestamp_utc": ISO8601DateFormatter().string(from: Date()),
        "platform": [
            "os": "macos",
            "architecture": architectureName(),
            "rosetta_translated": rosettaTranslated(),
            "version": ProcessInfo.processInfo.operatingSystemVersionString,
        ],
        "identity": [
            "runner_bundle_id": actualRunnerBundleID ?? "unavailable",
            "fixture_bundle_id": fixtureBundleID,
            "launcher_declared_identity_stability": identityStability,
        ],
        "permissions": [
            "accessibility": [
                "trusted": accessibilityTrusted,
                "prompt_requested": promptRequested,
            ],
            "screen_capture": [
                "preflight_granted": screenCaptureGranted,
                "request_attempted": false,
                "capture_attempted": false,
            ],
        ],
        "limits": [
            "target_scope": "fixture_process_only",
            "screen_content_collected": false,
        ],
        "checks": checks,
        "summary": ["passed": passed, "failed": failed, "total": checks.count],
    ]
}

private func run(arguments parsedArguments: Arguments? = nil) -> Outcome {
    guard let args = parsedArguments ?? parseArguments() else {
        let report = makeReport(
            status: "failed",
            message: "参数无效：必须提供 --fixture-app <path>",
            promptRequested: false,
            accessibilityTrusted: false,
            screenCaptureGranted: false,
            checks: [["id": "arguments", "status": "fail", "message": "invalid arguments"]]
        )
        return Outcome(report: report, exitCode: 2)
    }
    reportIdentityStability = args.identityStability
    reportSourceRevision = args.sourceRevision
    reportSourceWorktree = args.sourceWorktree
    reportSourcePackageDigest = args.sourcePackageDigest
    guard writePIDFile(args.pidFile) else {
        return Outcome(
            report: makeReport(
                status: "failed", message: "runner pid file write failed",
                promptRequested: false, accessibilityTrusted: false,
                screenCaptureGranted: false, checks: [[
                    "id": "pid_file", "status": "fail",
                    "message": "无法原子写入 watchdog PID 文件",
                ]]
            ), exitCode: 1
        )
    }
    if let cancelFile = args.cancelFile,
       FileManager.default.fileExists(atPath: cancelFile.path) {
        return Outcome(
            report: makeReport(
                status: "failed", message: "runner launch was cancelled",
                promptRequested: false, accessibilityTrusted: false,
                screenCaptureGranted: false, checks: [[
                    "id": "watchdog_cancel", "status": "fail",
                    "message": "runner 在 LaunchServices 延迟期间被 watchdog 取消",
                ]]
            ), exitCode: 1
        )
    }

    let screenCaptureGranted = CGPreflightScreenCaptureAccess()
    guard processGroupConfigured else {
        return Outcome(
            report: makeReport(
                status: "failed", message: "runner process group setup failed",
                promptRequested: false, accessibilityTrusted: false,
                screenCaptureGranted: screenCaptureGranted, checks: [[
                    "id": "process_group", "status": "fail",
                    "message": "无法建立可由 watchdog 清理的独立进程组",
                ]]
            ), exitCode: 1
        )
    }
    guard Bundle.main.bundleIdentifier == runnerBundleID else {
        return Outcome(
            report: makeReport(
                status: "failed",
                message: "runner bundle identity validation failed",
                promptRequested: false,
                accessibilityTrusted: false,
                screenCaptureGranted: screenCaptureGranted,
                checks: [[
                    "id": "runner_identity",
                    "status": "fail",
                    "message": "实际 runner bundle ID 缺失或不符合预期",
                ]]
            ),
            exitCode: 1
        )
    }
    if args.promptAccessibility {
        let options = [
            kAXTrustedCheckOptionPrompt.takeUnretainedValue() as String: true
        ] as CFDictionary
        // The prompt is asynchronous.  Its return value is not treated as a
        // completed authorization; the plain trust probe below remains the
        // source of truth for this run.
        _ = AXIsProcessTrustedWithOptions(options)
    }
    let accessibilityTrusted = AXIsProcessTrusted()

    var checks: [[String: Any]] = [[
        "id": "screen_capture_preflight",
        "status": "pass",
        "message": "仅检查授权状态；未请求授权，也未截图",
        "evidence": ["granted": screenCaptureGranted, "capture_attempted": false],
    ]]

    guard accessibilityTrusted else {
        checks.append([
            "id": "accessibility_trust",
            "status": "unsupported",
            "message": args.promptAccessibility
                ? "辅助功能授权尚不可用；完成系统设置后重新运行"
                : "辅助功能未授权；默认未弹窗，可用 --prompt-accessibility 明确请求",
        ])
        return Outcome(
            report: makeReport(
                status: "unsupported",
                message: "macOS Accessibility 权限不可用，未启动 fixture",
                promptRequested: args.promptAccessibility,
                accessibilityTrusted: false,
                screenCaptureGranted: screenCaptureGranted,
                checks: checks
            ),
            exitCode: 3
        )
    }
    checks.append([
        "id": "accessibility_trust",
        "status": "pass",
        "message": "AXIsProcessTrusted 返回 true",
    ])

    guard Bundle(url: args.fixtureApp)?.bundleIdentifier == fixtureBundleID else {
        checks.append([
            "id": "fixture_identity",
            "status": "fail",
            "message": "fixture bundle ID 不符合预期",
        ])
        return Outcome(
            report: makeReport(
                status: "failed",
                message: "fixture identity validation failed",
                promptRequested: args.promptAccessibility,
                accessibilityTrusted: true,
                screenCaptureGranted: screenCaptureGranted,
                checks: checks
            ),
            exitCode: 1
        )
    }

    let executable = args.fixtureApp.appendingPathComponent("Contents/MacOS/AiAutoDesktopAXFixture")
    guard FileManager.default.isExecutableFile(atPath: executable.path) else {
        checks.append([
            "id": "fixture_executable",
            "status": "fail",
            "message": "fixture executable missing",
        ])
        return Outcome(
            report: makeReport(
                status: "failed",
                message: "fixture executable unavailable",
                promptRequested: args.promptAccessibility,
                accessibilityTrusted: true,
                screenCaptureGranted: screenCaptureGranted,
                checks: checks
            ),
            exitCode: 1
        )
    }

    let fixture = Process()
    fixture.executableURL = executable
    fixture.arguments = ["--parent-pid", String(getpid())]
    fixture.standardOutput = FileHandle.nullDevice
    fixture.standardError = FileHandle.nullDevice
    do {
        try fixture.run()
    } catch {
        checks.append(["id": "fixture_launch", "status": "fail", "message": "fixture launch failed"])
        return Outcome(
            report: makeReport(
                status: "failed",
                message: "无法启动 fixture",
                promptRequested: args.promptAccessibility,
                accessibilityTrusted: true,
                screenCaptureGranted: screenCaptureGranted,
                checks: checks
            ),
            exitCode: 1
        )
    }
    let fixturePID = fixture.processIdentifier
    guard fixturePID > 1, getpgid(fixturePID) == getpgrp() else {
        terminateAndReap(fixturePID)
        checks.append([
            "id": "fixture_process_group", "status": "fail",
            "message": "fixture 没有继承 runner 的受控进程组",
        ])
        return Outcome(report: makeReport(
            status: "failed", message: "fixture process group invalid",
            promptRequested: args.promptAccessibility, accessibilityTrusted: true,
            screenCaptureGranted: screenCaptureGranted, checks: checks
        ), exitCode: 1)
    }

    defer {
        terminateAndReap(fixturePID)
    }

    _ = NSRunningApplication(processIdentifier: fixturePID)?.activate(options: [.activateIgnoringOtherApps])
    let appElement = AXUIElementCreateApplication(fixturePID)
    let appTimeoutError = AXUIElementSetMessagingTimeout(
        appElement, axMessageTimeout
    )
    guard appTimeoutError == .success else {
        checks.append([
            "id": "ax_messaging_timeout",
            "status": "fail",
            "message": "无法设置 AX 消息超时",
            "evidence": ["ax_error": appTimeoutError.rawValue],
        ])
        return Outcome(
            report: makeReport(
                status: "failed", message: "AX timeout setup failed",
                promptRequested: args.promptAccessibility, accessibilityTrusted: true,
                screenCaptureGranted: screenCaptureGranted, checks: checks
            ), exitCode: 1
        )
    }

    func freshSnapshot() -> Snapshot {
        snapshot(appElement, maxDepth: args.maxDepth, maxNodes: args.maxNodes)
    }
    func findOne(_ current: Snapshot, identifier: String) -> Node? {
        let matches = current.nodes.filter { $0.identifier == identifier }
        return matches.count == 1 ? matches[0] : nil
    }
    func freshNode(identifier: String, role: String) -> Node? {
        let current = freshSnapshot()
        guard !current.truncated,
              current.axErrors.isEmpty,
              let node = findOne(current, identifier: identifier),
              node.role == role else { return nil }
        return node
    }

    var initial: Snapshot?
    let launchDeadline = Date().addingTimeInterval(10)
    while Date() < launchDeadline {
        let candidate = freshSnapshot()
        if findOne(candidate, identifier: "fixture-input") != nil,
           findOne(candidate, identifier: "fixture-secure-input") != nil,
           findOne(candidate, identifier: "fixture-apply") != nil,
           findOne(candidate, identifier: "fixture-status") != nil,
           findOne(candidate, identifier: "fixture-pointer") != nil,
           findOne(candidate, identifier: "fixture-pointer-status") != nil {
            initial = candidate
            break
        }
        if kill(fixturePID, 0) != 0 { break }
        Thread.sleep(forTimeInterval: 0.1)
    }

    guard let initial,
          let initialInput = findOne(initial, identifier: "fixture-input"),
          let initialSecureInput = findOne(initial, identifier: "fixture-secure-input"),
          let initialButton = findOne(initial, identifier: "fixture-apply"),
          let initialStatus = findOne(initial, identifier: "fixture-status"),
          let initialPointerButton = findOne(initial, identifier: "fixture-pointer"),
          let initialPointerStatus = findOne(
              initial, identifier: "fixture-pointer-status"
          ) else {
        checks.append(["id": "bounded_discovery", "status": "fail", "message": "未在时限内唯一定位六个 fixture 控件"])
        return Outcome(
            report: makeReport(
                status: "failed",
                message: "AX fixture discovery failed",
                promptRequested: args.promptAccessibility,
                accessibilityTrusted: true,
                screenCaptureGranted: screenCaptureGranted,
                checks: checks
            ),
            exitCode: 1
        )
    }
    checks.append([
        "id": "bounded_discovery",
        "status": initial.truncated ? "fail" : "pass",
        "message": initial.truncated ? "AX 遍历触及边界" : "在 fixture 进程内完成有界遍历和唯一定位",
        "evidence": ["nodes_read": initial.nodes.count, "max_depth": args.maxDepth, "max_nodes": args.maxNodes],
    ])
    guard !initial.truncated else {
        return Outcome(
            report: makeReport(
                status: "failed",
                message: "bounded traversal truncated",
                promptRequested: args.promptAccessibility,
                accessibilityTrusted: true,
                screenCaptureGranted: screenCaptureGranted,
                checks: checks
            ),
            exitCode: 1
        )
    }
    let secureRoleAccepted = initialSecureInput.role == kAXTextFieldRole as String
        || initialSecureInput.role == "AXSecureTextField"
    guard initial.axErrors.isEmpty,
          initialInput.role == kAXTextFieldRole as String,
          secureRoleAccepted,
          initialButton.role == kAXButtonRole as String,
          initialStatus.role == kAXStaticTextRole as String,
          initialPointerButton.role == kAXButtonRole as String,
          initialPointerStatus.role == kAXStaticTextRole as String else {
        checks.append([
            "id": "roles_and_ax_errors",
            "status": "fail",
            "message": "fixture role 不符合预期或 AX 读取发生错误",
            "evidence": ["ax_errors": initial.axErrors],
        ])
        return Outcome(
            report: makeReport(
                status: "failed", message: "fixture semantics invalid",
                promptRequested: args.promptAccessibility, accessibilityTrusted: true,
                screenCaptureGranted: screenCaptureGranted, checks: checks
            ), exitCode: 1
        )
    }
    let windowCount = initial.nodes.filter {
        $0.role == kAXWindowRole as String
    }.count
    guard windowCount == 1 else {
        checks.append([
            "id": "window_role", "status": "fail",
            "message": "fixture 必须唯一暴露一个 AXWindow",
            "evidence": ["candidate_count": windowCount],
        ])
        return Outcome(report: makeReport(
            status: "failed", message: "window role validation failed",
            promptRequested: args.promptAccessibility, accessibilityTrusted: true,
            screenCaptureGranted: screenCaptureGranted, checks: checks
        ), exitCode: 1)
    }
    let duplicateTitleCount = initial.nodes.filter {
        $0.role == kAXButtonRole as String && $0.title == "Apply Fixture Value"
    }.count
    guard duplicateTitleCount == 2 else {
        checks.append([
            "id": "ambiguous_visible_title",
            "status": "fail",
            "message": "同名按钮歧义 fixture 未按预期暴露",
            "evidence": ["candidate_count": duplicateTitleCount],
        ])
        return Outcome(
            report: makeReport(
                status: "failed", message: "ambiguity fixture invalid",
                promptRequested: args.promptAccessibility, accessibilityTrusted: true,
                screenCaptureGranted: screenCaptureGranted, checks: checks
            ), exitCode: 1
        )
    }
    checks.append([
        "id": "roles_and_ambiguity", "status": "pass",
        "message": "角色正确，且可见标题存在两个候选；identifier 可唯一定位",
    ])

    let secureRejected = !typeTextTargetIsEligible(initialSecureInput)
    checks.append([
        "id": "type_text_secure_rejected",
        "status": secureRejected ? "pass" : "fail",
        "message": "secure text 目标在任何键盘事件前被 role/subrole 检查拒绝",
        "evidence": [
            "event_post_attempted": false,
            "protected": secureRejected,
            "value_read": false,
        ],
    ])
    guard secureRejected else {
        return Outcome(report: makeReport(
            status: "failed", message: "secure text rejection verification failed",
            promptRequested: args.promptAccessibility, accessibilityTrusted: true,
            screenCaptureGranted: screenCaptureGranted, checks: checks
        ), exitCode: 1)
    }

    guard let focusInput = freshNode(
        identifier: "fixture-input", role: kAXTextFieldRole as String
    ) else {
        return Outcome(report: makeReport(
            status: "failed", message: "focus target re-resolve failed",
            promptRequested: args.promptAccessibility, accessibilityTrusted: true,
            screenCaptureGranted: screenCaptureGranted, checks: checks
        ), exitCode: 1)
    }
    let (focusPreflightError, focusSettable) = settable(
        focusInput.element, kAXFocusedAttribute as CFString
    )
    guard focusPreflightError == .success && focusSettable else {
        checks.append(["id": "focus_preflight", "status": "fail", "message": "AXFocused 不可写", "evidence": ["ax_error": focusPreflightError.rawValue, "settable": focusSettable]])
        return Outcome(report: makeReport(status: "failed", message: "focus preflight failed", promptRequested: args.promptAccessibility, accessibilityTrusted: true, screenCaptureGranted: screenCaptureGranted, checks: checks), exitCode: 1)
    }
    let focusError = AXUIElementSetAttributeValue(
        focusInput.element, kAXFocusedAttribute as CFString, kCFBooleanTrue
    )
    Thread.sleep(forTimeInterval: 0.1)
    let afterFocus = freshSnapshot()
    let focused = !afterFocus.truncated
        && afterFocus.axErrors.isEmpty
        && findOne(afterFocus, identifier: "fixture-input")?.focused == true
    checks.append([
        "id": "focus_and_reread",
        "status": focusError == .success && focused ? "pass" : "fail",
        "message": "设置焦点后重新读取 AXFocused",
        "evidence": ["ax_error": focusError.rawValue, "focused_after": focused],
    ])
    guard focusError == .success && focused else {
        return Outcome(
            report: makeReport(
                status: "failed", message: "focus verification failed",
                promptRequested: args.promptAccessibility, accessibilityTrusted: true,
                screenCaptureGranted: screenCaptureGranted, checks: checks
            ), exitCode: 1
        )
    }

    guard let valueInput = freshNode(
        identifier: "fixture-input", role: kAXTextFieldRole as String
    ) else {
        return Outcome(report: makeReport(status: "failed", message: "value target re-resolve failed", promptRequested: args.promptAccessibility, accessibilityTrusted: true, screenCaptureGranted: screenCaptureGranted, checks: checks), exitCode: 1)
    }
    let (valuePreflightError, valueSettable) = settable(
        valueInput.element, kAXValueAttribute as CFString
    )
    guard valuePreflightError == .success && valueSettable else {
        checks.append(["id": "set_value_preflight", "status": "fail", "message": "AXValue 不可写", "evidence": ["ax_error": valuePreflightError.rawValue, "settable": valueSettable]])
        return Outcome(report: makeReport(status: "failed", message: "value preflight failed", promptRequested: args.promptAccessibility, accessibilityTrusted: true, screenCaptureGranted: screenCaptureGranted, checks: checks), exitCode: 1)
    }
    let valueError = AXUIElementSetAttributeValue(
        valueInput.element, kAXValueAttribute as CFString, testValue as CFString
    )
    Thread.sleep(forTimeInterval: 0.1)
    let afterValue = freshSnapshot()
    let updatedValue = !afterValue.truncated && afterValue.axErrors.isEmpty
        ? findOne(afterValue, identifier: "fixture-input")?.value
        : nil
    checks.append([
        "id": "set_value_and_reread",
        "status": valueError == .success && updatedValue == testValue ? "pass" : "fail",
        "message": "写入 AXValue 后从新快照读取固定测试值",
        "evidence": ["ax_error": valueError.rawValue, "value_matches": updatedValue == testValue],
    ])
    guard valueError == .success && updatedValue == testValue else {
        return Outcome(
            report: makeReport(
                status: "failed", message: "set value verification failed",
                promptRequested: args.promptAccessibility, accessibilityTrusted: true,
                screenCaptureGranted: screenCaptureGranted, checks: checks
            ), exitCode: 1
        )
    }

    let typeTextSegments = ["ASCII ", "中文 ", "😀"]
    var expectedTypedValue = ""
    var typeTextPassed = true
    var typeTextEvidence: [[String: Any]] = []
    for (index, segment) in typeTextSegments.enumerated() {
        guard let typeInput = freshNode(
            identifier: "fixture-input", role: kAXTextFieldRole as String
        ), typeTextTargetIsEligible(typeInput) else {
            typeTextPassed = false
            typeTextEvidence.append([
                "segment_index": index, "fresh_target": false,
                "value_matches": false,
            ])
            break
        }
        let (typeValuePreflightError, typeValueSettable) = settable(
            typeInput.element, kAXValueAttribute as CFString
        )
        let (typeFocusPreflightError, typeFocusSettable) = settable(
            typeInput.element, kAXFocusedAttribute as CFString
        )
        guard typeValuePreflightError == .success && typeValueSettable,
              typeFocusPreflightError == .success && typeFocusSettable else {
            typeTextPassed = false
            typeTextEvidence.append([
                "segment_index": index, "fresh_target": true,
                "value_settable": typeValueSettable,
                "value_ax_error": typeValuePreflightError.rawValue,
                "focus_settable": typeFocusSettable,
                "focus_ax_error": typeFocusPreflightError.rawValue,
            ])
            break
        }
        var clearError = AXError.success
        if index == 0 {
            clearError = AXUIElementSetAttributeValue(
                typeInput.element, kAXValueAttribute as CFString, "" as CFString
            )
        }
        let typeFocusError = AXUIElementSetAttributeValue(
            typeInput.element, kAXFocusedAttribute as CFString, kCFBooleanTrue
        )
        Thread.sleep(forTimeInterval: 0.05)
        let beforeType = freshSnapshot()
        let focusVerified = !beforeType.truncated
            && beforeType.axErrors.isEmpty
            && findOne(beforeType, identifier: "fixture-input")?.focused == true
        let frontmost = NSWorkspace.shared.frontmostApplication?.processIdentifier == fixturePID
        let secureEventInputEnabled = IsSecureEventInputEnabled() != 0
        var dispatch = TypeTextDispatch(submitted: false, utf16UnitsPosted: 0)
        if clearError == .success && typeFocusError == .success
            && focusVerified && frontmost && !secureEventInputEnabled {
            dispatch = postUnicodeText(segment, to: fixturePID)
        }
        expectedTypedValue += segment
        var observedValue: String?
        var postconditionErrors: [Int] = []
        let typeDeadline = Date().addingTimeInterval(2)
        while Date() < typeDeadline {
            let afterType = freshSnapshot()
            postconditionErrors = afterType.axErrors
            observedValue = nil
            if !afterType.truncated && afterType.axErrors.isEmpty {
                observedValue = findOne(
                    afterType, identifier: "fixture-input"
                )?.value
            }
            if observedValue == expectedTypedValue { break }
            Thread.sleep(forTimeInterval: 0.05)
        }
        let segmentPassed = dispatch.submitted && observedValue == expectedTypedValue
        typeTextEvidence.append([
            "segment_index": index,
            "kind": index == 0 ? "ascii" : index == 1 ? "cjk" : "non_bmp",
            "fresh_target": true,
            "focus_verified_before_dispatch": focusVerified,
            "frontmost_before_dispatch": frontmost,
            "secure_event_input_enabled_before_dispatch": secureEventInputEnabled,
            "secure_event_input_checked_before_dispatch": true,
            "event_submitted": dispatch.submitted,
            "utf16_units_posted": dispatch.utf16UnitsPosted,
            "value_matches_from_fresh_snapshot": observedValue == expectedTypedValue,
            "postcondition_ax_errors": postconditionErrors,
        ])
        if !segmentPassed {
            typeTextPassed = false
            break
        }
    }
    typeTextPassed = typeTextPassed && expectedTypedValue == typeTextValue
    checks.append([
        "id": "type_text_unicode_and_reread",
        "status": typeTextPassed ? "pass" : "fail",
        "message": "逐段发送 ASCII、中文和非 BMP Unicode，并分别从新快照验证累计值",
        "evidence": [
            "cases": typeTextEvidence,
            "expected_utf16_units": typeTextValue.utf16.count,
        ],
    ])
    guard typeTextPassed else {
        return Outcome(report: makeReport(
            status: "failed", message: "type text verification failed",
            promptRequested: args.promptAccessibility, accessibilityTrusted: true,
            screenCaptureGranted: screenCaptureGranted, checks: checks
        ), exitCode: 1)
    }

    guard let pressButton = freshNode(
        identifier: "fixture-apply", role: kAXButtonRole as String
    ) else {
        return Outcome(report: makeReport(status: "failed", message: "press target re-resolve failed", promptRequested: args.promptAccessibility, accessibilityTrusted: true, screenCaptureGranted: screenCaptureGranted, checks: checks), exitCode: 1)
    }
    let (actionPreflightError, supportedActions) = actions(pressButton.element)
    let pressSupported = supportedActions.contains(kAXPressAction as String)
    guard actionPreflightError == .success && pressSupported else {
        checks.append(["id": "press_preflight", "status": "fail", "message": "AXPress 不受支持", "evidence": ["ax_error": actionPreflightError.rawValue, "supported": pressSupported]])
        return Outcome(report: makeReport(status: "failed", message: "press preflight failed", promptRequested: args.promptAccessibility, accessibilityTrusted: true, screenCaptureGranted: screenCaptureGranted, checks: checks), exitCode: 1)
    }
    let pressError = AXUIElementPerformAction(
        pressButton.element, kAXPressAction as CFString
    )
    var observedStatus: String? = initialStatus.value
    var lastPostconditionErrors: [Int] = []
    let pressDeadline = Date().addingTimeInterval(3)
    while Date() < pressDeadline {
        let current = freshSnapshot()
        lastPostconditionErrors = current.axErrors
        observedStatus = nil
        if !current.truncated && current.axErrors.isEmpty {
            observedStatus = findOne(
                current, identifier: "fixture-status"
            )?.value
        }
        if observedStatus == expectedStatus { break }
        Thread.sleep(forTimeInterval: 0.05)
    }
    let pressVerified = pressError == .success && observedStatus == expectedStatus
    checks.append([
        "id": "press_and_reread",
        "status": pressVerified ? "pass" : "fail",
        "message": "执行 AXPress 后从新快照验证状态文本",
        "evidence": [
            "ax_error": pressError.rawValue,
            "status_matches": observedStatus == expectedStatus,
            "postcondition_ax_errors": lastPostconditionErrors,
        ],
    ])
    guard pressVerified else {
        return Outcome(
            report: makeReport(
                status: "failed", message: "press verification failed",
                promptRequested: args.promptAccessibility, accessibilityTrusted: true,
                screenCaptureGranted: screenCaptureGranted, checks: checks
            ),
            exitCode: 1
        )
    }

    // pointer_click uses only a freshly resolved element in the owned fixture.
    // Its center is derived from AX bounds; there are no screen pixels, image recognition, or
    // caller-provided/raw desktop coordinates in this path.
    let beforePointer = freshSnapshot()
    guard !beforePointer.truncated, beforePointer.axErrors.isEmpty,
          let pointerButton = findOne(
              beforePointer, identifier: "fixture-pointer"
          ), pointerButton.role == kAXButtonRole as String,
          let pointerStatusBefore = findOne(
              beforePointer, identifier: "fixture-pointer-status"
          ), pointerStatusBefore.role == kAXStaticTextRole as String else {
        checks.append([
            "id": "pointer_click_and_reread", "status": "fail",
            "message": "pointer target fresh AX 唯一定位失败",
            "evidence": [
                "event_submitted": false,
                "fresh_target": false,
                "postcondition_reread": false,
            ],
        ])
        return Outcome(report: makeReport(
            status: "failed", message: "pointer target re-resolve failed",
            promptRequested: args.promptAccessibility, accessibilityTrusted: true,
            screenCaptureGranted: screenCaptureGranted, checks: checks
        ), exitCode: 1)
    }
    var pointerBoundsErrors: [Int] = []
    let pointerBounds = elementBounds(
        pointerButton.element, errors: &pointerBoundsErrors
    )
    let positiveAreaBounds = pointerBounds.map {
        $0.width > 0 && $0.height > 0
    } ?? false
    var pointerTargetPID = pid_t(0)
    let pointerPIDError = AXUIElementGetPid(
        pointerButton.element, &pointerTargetPID
    )
    let targetPIDMatches = pointerPIDError == .success
        && pointerTargetPID == fixturePID
    let frontmostBeforeDispatch =
        NSWorkspace.shared.frontmostApplication?.processIdentifier == fixturePID
    let pointerStatusIdleBeforeDispatch =
        pointerStatusBefore.value == expectedInitialPointerStatus
    var pointerHitTestMatchesTarget = false
    var pointerHitTestErrorCode = -1
    var pointerDispatch = PointerClickDispatch(
        submitted: false, frontmostAtDispatch: false
    )
    var pointerCenterFinite = false
    if let bounds = pointerBounds, positiveAreaBounds, targetPIDMatches,
       frontmostBeforeDispatch, pointerStatusIdleBeforeDispatch {
        let center = CGPoint(x: bounds.midX, y: bounds.midY)
        pointerCenterFinite = center.x.isFinite && center.y.isFinite
        var hitElement: AXUIElement?
        let pointerHitTestError = AXUIElementCopyElementAtPosition(
            appElement, Float(center.x), Float(center.y), &hitElement
        )
        pointerHitTestErrorCode = Int(pointerHitTestError.rawValue)
        pointerHitTestMatchesTarget = pointerHitTestError == .success
            && hitElement.map { CFEqual($0, pointerButton.element) } == true
        if pointerCenterFinite && pointerHitTestMatchesTarget {
            pointerDispatch = postLeftClick(
                at: center, to: pointerTargetPID
            )
        }
    }

    var observedPointerStatus: String?
    var pointerPostconditionErrors: [Int] = []
    var pointerPostconditionFresh = false
    if pointerDispatch.submitted {
        let pointerDeadline = Date().addingTimeInterval(3)
        while Date() < pointerDeadline {
            let afterPointer = freshSnapshot()
            pointerPostconditionErrors = afterPointer.axErrors
            pointerPostconditionFresh = !afterPointer.truncated
                && afterPointer.axErrors.isEmpty
            observedPointerStatus = pointerPostconditionFresh
                ? findOne(
                    afterPointer, identifier: "fixture-pointer-status"
                )?.value : nil
            if observedPointerStatus == expectedPointerStatus { break }
            Thread.sleep(forTimeInterval: 0.05)
        }
    }
    let pointerVerified = pointerDispatch.submitted
        && pointerPostconditionFresh
        && observedPointerStatus == expectedPointerStatus
    checks.append([
        "id": "pointer_click_and_reread",
        "status": pointerVerified ? "pass" : "fail",
        "message": "从 fresh AX 唯一目标的正面积 bounds 计算中心点，向 fixture PID 提交 CGEvent 左键点击并重新读取状态",
        "evidence": [
            "fresh_target": true,
            "positive_area_bounds": positiveAreaBounds,
            "bounds_ax_errors": pointerBoundsErrors,
            "target_pid_matches_fixture": targetPIDMatches,
            "pid_ax_error": pointerPIDError.rawValue,
            "frontmost_before_dispatch": frontmostBeforeDispatch,
            "frontmost_at_dispatch": pointerDispatch.frontmostAtDispatch,
            "status_idle_before_dispatch": pointerStatusIdleBeforeDispatch,
            "center_derived_from_ax_bounds": true,
            "center_finite": pointerCenterFinite,
            "hit_test_matches_target": pointerHitTestMatchesTarget,
            "hit_test_ax_error": pointerHitTestErrorCode,
            "event_submitted": pointerDispatch.submitted,
            "button": "left",
            "position": "center",
            "postcondition_reread": pointerPostconditionFresh,
            "status_matches_from_fresh_snapshot": observedPointerStatus == expectedPointerStatus,
            "postcondition_ax_errors": pointerPostconditionErrors,
        ],
    ])

    return Outcome(
        report: makeReport(
            status: pointerVerified ? "passed" : "failed",
            message: pointerVerified ? "macOS AX fixture 测试通过" : "pointer click verification failed",
            promptRequested: args.promptAccessibility,
            accessibilityTrusted: true,
            screenCaptureGranted: screenCaptureGranted,
            checks: checks
        ),
        exitCode: pointerVerified ? 0 : 1
    )
}

private func emit(_ report: [String: Any], to destination: URL?) -> Bool {
    guard JSONSerialization.isValidJSONObject(report),
          let data = try? JSONSerialization.data(withJSONObject: report, options: [.sortedKeys]) else {
        FileHandle.standardOutput.write(Data("{\"kind\":\"macos_ax_fixture_test\",\"status\":\"failed\",\"message\":\"JSON serialization failed\"}\n".utf8))
        return false
    }
    guard let destination = destination else {
        FileHandle.standardOutput.write(data)
        FileHandle.standardOutput.write(Data([0x0a]))
        return true
    }
    let temporary = destination.deletingLastPathComponent().appendingPathComponent(
        destination.lastPathComponent + ".writing-" + UUID().uuidString
    )
    do {
        try data.write(to: temporary, options: .atomic)
        if FileManager.default.fileExists(atPath: destination.path) {
            try FileManager.default.removeItem(at: destination)
        }
        try FileManager.default.moveItem(at: temporary, to: destination)
        return true
    } catch {
        try? FileManager.default.removeItem(at: temporary)
        return false
    }
}

let processGroupConfigured = configureProcessGroup()
let parsedArguments = parseArguments()
let outcome = run(arguments: parsedArguments)
let reportDestination = parsedArguments?.reportPath
exit(emit(outcome.report, to: reportDestination) ? outcome.exitCode : 1)

