import AppKit
import ApplicationServices
import Carbon.HIToolbox
import Darwin
import Foundation

private let protocolVersion = 2
private let maximumInputBytes = 1_048_576
private let maximumOutputBytes = 8 * 1_024 * 1_024 - 1
private let maximumFieldCharacters = 4_096
private let maximumTypeTextScalars = 1_024
private let maximumTypeTextUTF16Units = maximumTypeTextScalars * 2
private let maximumUnicodeUnitsPerEvent = 20
private let maximumDepth = 128
private let maximumNodes = 5_000

private struct HelperFailure: Error {
    let code: String
    let message: String
    let retryable: Bool
    let data: [String: Any]

    init(
        _ code: String,
        _ message: String,
        retryable: Bool = false,
        data: [String: Any] = [:]
    ) {
        self.code = code
        self.message = message
        self.retryable = retryable
        self.data = data
    }
}

private final class RequestProgress {
    private(set) var keyboardDispatchStarted = false
    private(set) var pointerDispatchStarted = false
    private(set) var focusChanged = false

    func markFocusChanged() {
        focusChanged = true
    }

    func markKeyboardDispatchStarted() {
        keyboardDispatchStarted = true
    }

    func markPointerDispatchStarted() {
        pointerDispatchStarted = true
    }

    func keyboardMetadata(phase: String) -> [String: Any] {
        [
            "phase": phase,
            "keyboard_dispatch_started": keyboardDispatchStarted,
            "focus_changed": focusChanged,
        ]
    }

    func pointerMetadata(phase: String) -> [String: Any] {
        [
            "phase": phase,
            "pointer_dispatch_started": pointerDispatchStarted,
        ]
    }
}

private struct Arguments {
    let values: [String: Any]

    func only(_ allowed: Set<String>) throws {
        let unknown = Set(values.keys).subtracting(allowed).sorted()
        guard unknown.isEmpty else {
            throw HelperFailure(
                "DRIVER.INVALID_REQUEST",
                "helper args contains unsupported fields",
                data: ["fields": unknown]
            )
        }
    }

    func string(_ name: String, required: Bool = true) throws -> String? {
        guard let raw = values[name] else {
            if required {
                throw HelperFailure("DRIVER.INVALID_REQUEST", "helper args.\(name) is required")
            }
            return nil
        }
        guard let value = raw as? String, value.count <= maximumFieldCharacters else {
            throw HelperFailure("DRIVER.INVALID_REQUEST", "helper args.\(name) must be a bounded string")
        }
        return value
    }

    func integer(_ name: String, minimum: Int, maximum: Int) throws -> Int {
        guard let number = values[name] as? NSNumber, CFGetTypeID(number) != CFBooleanGetTypeID() else {
            throw HelperFailure("DRIVER.INVALID_REQUEST", "helper args.\(name) must be an integer")
        }
        let doubleValue = number.doubleValue
        let value = number.intValue
        guard doubleValue.isFinite, Double(value) == doubleValue, value >= minimum, value <= maximum else {
            throw HelperFailure("DRIVER.INVALID_REQUEST", "helper args.\(name) is outside its allowed range")
        }
        return value
    }

    func object(_ name: String) throws -> [String: Any] {
        guard let value = values[name] as? [String: Any] else {
            throw HelperFailure("DRIVER.INVALID_REQUEST", "helper args.\(name) must be an object")
        }
        return value
    }
}

private struct NativeNode {
    let element: AXUIElement
    let parentIndex: Int?
    let role: String
    let subrole: String?
    let name: String?
    let detail: String?
    let value: String?
    let states: [String: Any]
    let bounds: [String: Int]?
    let actions: [String]
    let provenance: [String: Any]
}

private final class TokenStore {
    private struct Entry {
        let generation: Int
        let element: AXUIElement
    }

    private var generation = 0
    private var entries: [String: Entry] = [:]

    func beginSnapshot() {
        generation += 1
        let oldest = generation - 1
        entries = entries.filter { $0.value.generation >= oldest }
    }

    func insert(_ element: AXUIElement, index: Int) -> String {
        let token = "g\(generation)-n\(index)-\(UUID().uuidString.lowercased())"
        entries[token] = Entry(generation: generation, element: element)
        return token
    }

    func resolve(_ token: String) throws -> AXUIElement {
        guard let entry = entries[token] else {
            throw HelperFailure(
                "DRIVER.STALE_SNAPSHOT",
                "native AX token is no longer available",
                data: ["reason": "native_token_expired"]
            )
        }
        return entry.element
    }
}

private final class AXService {
    private let tokens = TokenStore()

    func execute(
        operation: String, args: [String: Any], deadlineMS: Double?,
        requestID: String, progress: RequestProgress
    ) throws -> Any {
        try checkDeadline(deadlineMS)
        switch operation {
        case "status":
            try Arguments(values: args).only([])
            return [
                "protocol_version": protocolVersion,
                "implementation": "native_accessibility_api",
                "accessibility_trusted": AXIsProcessTrusted(),
                "process_id": Int(getpid()),
            ]
        case "list_apps":
            try Arguments(values: args).only([])
            return try listApps(deadlineMS: deadlineMS)
        case "snapshot":
            return try snapshot(args: Arguments(values: args), deadlineMS: deadlineMS)
        case "same_element":
            return try sameElement(args: Arguments(values: args), deadlineMS: deadlineMS)
        case "focus":
            return try focus(args: Arguments(values: args), deadlineMS: deadlineMS)
        case "invoke":
            return try invoke(args: Arguments(values: args), deadlineMS: deadlineMS)
        case "pointer_click":
            return try pointerClick(
                args: Arguments(values: args), deadlineMS: deadlineMS,
                requestID: requestID, progress: progress
            )
        case "set_value":
            return try setValue(args: Arguments(values: args), deadlineMS: deadlineMS)
        case "type_text":
            return try typeText(
                args: Arguments(values: args), deadlineMS: deadlineMS,
                requestID: requestID, progress: progress
            )
        default:
            throw HelperFailure(
                "DRIVER.INVALID_REQUEST",
                "unknown helper operation",
                data: ["operation": operation]
            )
        }
    }

    private func checkDeadline(_ deadlineMS: Double?) throws {
        guard let deadlineMS else { return }
        guard deadlineMS.isFinite else {
            throw HelperFailure("DRIVER.INVALID_REQUEST", "deadline_ms must be finite")
        }
        if Date().timeIntervalSince1970 * 1_000 >= deadlineMS {
            throw HelperFailure(
                "DRIVER.TIMEOUT",
                "AX helper request deadline elapsed",
                retryable: true,
                data: ["phase": "before_native_call", "effect": "not_applied"]
            )
        }
    }

    // Carbon's Boolean result was imported as UInt8 by older Swift SDKs and as
    // Bool by current SDKs. Keep both overloads so either importer is accepted.
    private func secureEventInputIsEnabled(_ value: Bool) -> Bool {
        return value
    }

    private func secureEventInputIsEnabled(_ value: UInt8) -> Bool {
        return value != 0
    }

    private func requireTrusted() throws {
        guard AXIsProcessTrusted() else {
            throw HelperFailure(
                "DRIVER.UNAVAILABLE",
                "Accessibility permission has not been granted to MacOSAXHelper.app",
                data: [
                    "reason": "accessibility_not_trusted",
                    "bundle_id": "dev.ai-auto-desktop.macos-ax-helper",
                ]
            )
        }
    }

    private func listApps(deadlineMS: Double?) throws -> [String: Any] {
        try checkDeadline(deadlineMS)
        let apps = NSWorkspace.shared.runningApplications.compactMap { app -> [String: Any]? in
            guard app.processIdentifier > 0, !app.isTerminated else { return nil }
            var result: [String: Any] = [
                "process_id": Int(app.processIdentifier),
                "active": app.isActive,
                "hidden": app.isHidden,
                "activation_policy": app.activationPolicy.rawValue,
            ]
            if let bundleID = app.bundleIdentifier { result["bundle_id"] = bounded(bundleID) }
            if let name = app.localizedName { result["name"] = bounded(name) }
            return result
        }.sorted { left, right in
            (left["process_id"] as? Int ?? 0) < (right["process_id"] as? Int ?? 0)
        }
        return [
            "accessibility_trusted": AXIsProcessTrusted(),
            "apps": apps,
        ]
    }

    private func selectApp(_ selector: [String: Any], deadlineMS: Double?) throws -> [String: Any] {
        let arguments = Arguments(values: selector)
        try arguments.only(["process_id", "bundle_id", "name"])
        guard !selector.isEmpty else {
            throw HelperFailure("DRIVER.INVALID_REQUEST", "app selector cannot be empty")
        }
        var normalized: [String: Any] = [:]
        if selector["process_id"] != nil {
            normalized["process_id"] = try arguments.integer("process_id", minimum: 1, maximum: Int(Int32.max))
        }
        for name in ["bundle_id", "name"] where selector[name] != nil {
            guard let value = try arguments.string(name), !value.isEmpty else {
                throw HelperFailure("DRIVER.INVALID_REQUEST", "app.\(name) cannot be empty")
            }
            normalized[name] = value
        }
        let listing = try listApps(deadlineMS: deadlineMS)
        let apps = listing["apps"] as? [[String: Any]] ?? []
        let matches = apps.filter { app in
            normalized.allSatisfy { key, value in
                if let expected = value as? Int { return app[key] as? Int == expected }
                return app[key] as? String == value as? String
            }
        }
        if matches.isEmpty {
            throw HelperFailure("DRIVER.NOT_FOUND", "app selector matched no running application", data: ["app": normalized])
        }
        if matches.count > 1 {
            throw HelperFailure(
                "DRIVER.AMBIGUOUS",
                "app selector matched more than one running application",
                data: ["candidate_count": matches.count]
            )
        }
        return matches[0]
    }

    private func snapshot(args: Arguments, deadlineMS: Double?) throws -> [String: Any] {
        try args.only(["app", "max_depth", "max_nodes"])
        try requireTrusted()
        let selector = try args.object("app")
        let maxDepth = try args.integer("max_depth", minimum: 0, maximum: maximumDepth)
        let maxNodes = try args.integer("max_nodes", minimum: 1, maximum: maximumNodes)
        let app = try selectApp(selector, deadlineMS: deadlineMS)
        guard let pid = app["process_id"] as? Int, pid > 0 else {
            throw HelperFailure("DRIVER.ACTION_FAILED", "selected app has no valid process id")
        }
        let root = AXUIElementCreateApplication(pid_t(pid))
        try configureTimeout(root, deadlineMS: deadlineMS)
        let captured = try capture(root, app: app, maxDepth: maxDepth, maxNodes: maxNodes, deadlineMS: deadlineMS)
        return [
            "app": app,
            "nodes": captured.nodes,
            "truncated": captured.truncated,
        ]
    }

    private func capture(
        _ root: AXUIElement,
        app: [String: Any],
        maxDepth: Int,
        maxNodes: Int,
        deadlineMS: Double?
    ) throws -> (nodes: [[String: Any]], truncated: Bool) {
        tokens.beginSnapshot()
        var queue: [(element: AXUIElement, parent: Int?, depth: Int)] = [(root, nil, 0)]
        var cursor = 0
        var output: [[String: Any]] = []
        var visited: [AXUIElement] = []
        var truncated = false
        while cursor < queue.count && output.count < maxNodes {
            try checkDeadline(deadlineMS)
            let item = queue[cursor]
            cursor += 1
            if visited.contains(where: { CFEqual($0, item.element) }) { continue }
            visited.append(item.element)
            let node = try readNode(item.element, parent: item.parent, app: app, deadlineMS: deadlineMS)
            let currentIndex = output.count
            let token = tokens.insert(item.element, index: currentIndex)
            var serialized: [String: Any] = [
                "native_token": token,
                "role": node.role,
                "states": node.states,
                "actions": node.actions,
                "provenance": node.provenance,
            ]
            serialized["parent_index"] = node.parentIndex.map { $0 as Any } ?? NSNull()
            serialized["subrole"] = node.subrole.map { $0 as Any } ?? NSNull()
            serialized["name"] = node.name.map { $0 as Any } ?? NSNull()
            serialized["description"] = node.detail.map { $0 as Any } ?? NSNull()
            serialized["value"] = node.value.map { $0 as Any } ?? NSNull()
            serialized["bounds"] = node.bounds.map { $0 as Any } ?? NSNull()
            output.append(serialized)

            let pending = queue.count - cursor
            let remaining = max(0, maxNodes - output.count - pending)
            let childResult = try children(item.element, limit: remaining, deadlineMS: deadlineMS)
            if childResult.truncated { truncated = true }
            if item.depth < maxDepth {
                queue.append(contentsOf: childResult.children.map { ($0, currentIndex, item.depth + 1) })
            } else if !childResult.children.isEmpty {
                truncated = true
            }
        }
        if cursor < queue.count { truncated = true }
        return (output, truncated)
    }

    private func readNode(
        _ element: AXUIElement,
        parent: Int?,
        app: [String: Any],
        deadlineMS: Double?
    ) throws -> NativeNode {
        try configureTimeout(element, deadlineMS: deadlineMS)
        let role = stringAttribute(element, kAXRoleAttribute as CFString) ?? "unknown"
        let subrole = stringAttribute(element, kAXSubroleAttribute as CFString)
        let protected = subrole == "AXSecureTextField" || role == "AXSecureTextField"
        let focusSettable = isSettable(element, kAXFocusedAttribute as CFString)
        let valueSettable = isSettable(element, kAXValueAttribute as CFString)
        let actionNames = copyActionNames(element)
        let enabled = boolAttribute(element, kAXEnabledAttribute as CFString)
        let normalizedBounds = copyBounds(element)
        var actions: [String] = []
        if focusSettable && enabled != false { actions.append("focus") }
        if let bounds = normalizedBounds, bounds["width", default: 0] > 0, bounds["height", default: 0] > 0,
           enabled != false && !protected {
            actions.append("pointer_click")
        }
        if actionNames.contains(kAXPressAction as String) && enabled != false { actions.append("invoke") }
        if valueSettable && !protected && enabled != false { actions.append("set_value") }
        if isKeyboardTextTarget(role: role) && focusSettable && !protected && enabled != false {
            actions.append("type_text")
        }
        var provenance: [String: Any] = [
            "ax_role": role,
            "process_id": app["process_id"] ?? NSNull(),
            "coordinate_space": "screen_points",
            "value_redacted": protected,
        ]
        if let identifier = stringAttribute(element, kAXIdentifierAttribute as CFString) {
            provenance["identifier"] = identifier
        }
        if let bundleID = app["bundle_id"] { provenance["bundle_id"] = bundleID }
        provenance["native_actions"] = actionNames.map(bounded)
        let title = stringAttribute(element, kAXTitleAttribute as CFString)
        let detail = stringAttribute(element, kAXDescriptionAttribute as CFString)
        let name = title ?? detail
        return NativeNode(
            element: element,
            parentIndex: parent,
            role: bounded(role),
            subrole: subrole.map(bounded),
            name: name.map(bounded),
            detail: detail.map(bounded),
            value: protected ? nil : scalarStringAttribute(element, kAXValueAttribute as CFString),
            states: [
                "enabled": enabled.map { $0 as Any } ?? NSNull(),
                "focused": boolAttribute(element, kAXFocusedAttribute as CFString).map { $0 as Any } ?? NSNull(),
                "focusable": focusSettable,
                "editable": valueSettable && !protected,
                "protected": protected,
            ],
            bounds: normalizedBounds,
            actions: actions.sorted(),
            provenance: provenance
        )
    }

    private func children(
        _ element: AXUIElement, limit: Int, deadlineMS: Double?
    ) throws -> (children: [AXUIElement], truncated: Bool) {
        try configureTimeout(element, deadlineMS: deadlineMS)
        var count: CFIndex = 0
        let countError = AXUIElementGetAttributeValueCount(
            element, kAXChildrenAttribute as CFString, &count
        )
        if countError == .attributeUnsupported || countError == .noValue { return ([], false) }
        guard countError == .success, count >= 0 else {
            return ([], true)
        }
        let requested = min(Int(count), max(0, limit))
        guard requested > 0 else { return ([], count > 0) }
        var raw: CFArray?
        let copyError = AXUIElementCopyAttributeValues(
            element, kAXChildrenAttribute as CFString, 0, requested, &raw
        )
        guard copyError == .success, let values = raw else { return ([], true) }
        var result: [AXUIElement] = []
        for index in 0..<CFArrayGetCount(values) {
            let rawPointer: UnsafeRawPointer? = CFArrayGetValueAtIndex(values, index)
            guard let pointer = rawPointer else { return (result, true) }
            let value = Unmanaged<CFTypeRef>.fromOpaque(pointer).takeUnretainedValue()
            guard CFGetTypeID(value) == AXUIElementGetTypeID() else { return (result, true) }
            result.append(unsafeBitCast(value, to: AXUIElement.self))
        }
        return (result, Int(count) > requested)
    }

    private func sameElement(args: Arguments, deadlineMS: Double?) throws -> [String: Any] {
        try args.only(["previous_token", "current_token"])
        try checkDeadline(deadlineMS)
        let previous = try tokens.resolve(try args.string("previous_token")!)
        let current = try tokens.resolve(try args.string("current_token")!)
        return ["same": CFEqual(previous, current)]
    }

    private func focus(args: Arguments, deadlineMS: Double?) throws -> [String: Any] {
        try args.only(["native_token"])
        try requireTrusted()
        let element = try tokens.resolve(try args.string("native_token")!)
        try configureTimeout(element, deadlineMS: deadlineMS)
        guard isSettable(element, kAXFocusedAttribute as CFString) else {
            throw HelperFailure("DRIVER.ACTION_UNSUPPORTED", "AXFocused is not settable", data: ["attribute": "AXFocused"])
        }
        try checkDeadline(deadlineMS)
        let error = AXUIElementSetAttributeValue(
            element, kAXFocusedAttribute as CFString, kCFBooleanTrue
        )
        try requireSuccess(error, operation: "AXUIElementSetAttributeValue(AXFocused)")
        return ["native_operation": "AXFocused", "accepted": true]
    }

    private func invoke(args: Arguments, deadlineMS: Double?) throws -> [String: Any] {
        try args.only(["native_token"])
        try requireTrusted()
        let element = try tokens.resolve(try args.string("native_token")!)
        try configureTimeout(element, deadlineMS: deadlineMS)
        guard copyActionNames(element).contains(kAXPressAction as String) else {
            throw HelperFailure("DRIVER.ACTION_UNSUPPORTED", "AXPress is not supported", data: ["action": "AXPress"])
        }
        try checkDeadline(deadlineMS)
        let error = AXUIElementPerformAction(element, kAXPressAction as CFString)
        try requireSuccess(error, operation: "AXUIElementPerformAction(AXPress)")
        return ["native_operation": "AXPress", "accepted": true]
    }

    private func pointerClick(
        args: Arguments, deadlineMS: Double?, requestID: String,
        progress: RequestProgress
    ) throws -> [String: Any] {
        try args.only(["native_token", "button", "position"])
        try requireTrusted()
        let button = try args.string("button")!
        guard button == "left" else {
            throw HelperFailure(
                "DRIVER.INVALID_REQUEST",
                "helper args.button must be left"
            )
        }
        let position = try args.string("position")!
        guard position == "center" else {
            throw HelperFailure(
                "DRIVER.INVALID_REQUEST",
                "helper args.position must be center"
            )
        }
        let element = try tokens.resolve(try args.string("native_token")!)
        try configureTimeout(element, deadlineMS: deadlineMS)
        let role = stringAttribute(element, kAXRoleAttribute as CFString)
        let subrole = stringAttribute(element, kAXSubroleAttribute as CFString)
        guard role != "AXSecureTextField", subrole != "AXSecureTextField" else {
            throw HelperFailure(
                "DRIVER.PROTECTED_ELEMENT",
                "protected element does not permit pointer_click",
                data: progress.pointerMetadata(phase: "target_preflight")
            )
        }
        guard boolAttribute(element, kAXEnabledAttribute as CFString) != false else {
            throw HelperFailure(
                "DRIVER.ACTION_UNSUPPORTED",
                "pointer target is disabled",
                data: progress.pointerMetadata(phase: "target_preflight")
            )
        }
        guard let bounds = copyBounds(element) else {
            throw HelperFailure(
                "DRIVER.ACTION_UNSUPPORTED",
                "pointer target has no usable bounds",
                data: progress.pointerMetadata(phase: "bounds_preflight")
            )
        }
        guard bounds["width", default: 0] > 0, bounds["height", default: 0] > 0 else {
            throw HelperFailure(
                "DRIVER.ACTION_UNSUPPORTED",
                "pointer target bounds must have positive area",
                data: progress.pointerMetadata(phase: "bounds_preflight")
            )
        }
        var targetPID = pid_t(0)
        let pidError = AXUIElementGetPid(element, &targetPID)
        try requireSuccess(pidError, operation: "AXUIElementGetPid")
        guard targetPID > 0 else {
            throw HelperFailure(
                "DRIVER.ACTION_FAILED",
                "pointer target has no valid process id"
            )
        }
        try requireFrontmost(targetPID, operation: "pointer_click")
        try checkDeadline(deadlineMS)

        let point = CGPoint(
            x: CGFloat(bounds["x", default: 0]) + CGFloat(bounds["width", default: 0]) / 2.0,
            y: CGFloat(bounds["y", default: 0]) + CGFloat(bounds["height", default: 0]) / 2.0
        )
        let application = AXUIElementCreateApplication(targetPID)
        try configureTimeout(application, deadlineMS: deadlineMS)
        var hitElement: AXUIElement?
        let hitError = AXUIElementCopyElementAtPosition(
            application, Float(point.x), Float(point.y), &hitElement
        )
        try requireSuccess(hitError, operation: "AXUIElementCopyElementAtPosition")
        guard let hitElement, CFEqual(hitElement, element) else {
            throw HelperFailure(
                "DRIVER.ACTION_FAILED",
                "pointer hit test no longer resolves to the target element",
                data: progress.pointerMetadata(phase: "hit_test_verification")
            )
        }
        guard let move = CGEvent(mouseEventSource: nil, mouseType: .mouseMoved, mouseCursorPosition: point, mouseButton: .left),
              let down = CGEvent(mouseEventSource: nil, mouseType: .leftMouseDown, mouseCursorPosition: point, mouseButton: .left),
              let up = CGEvent(mouseEventSource: nil, mouseType: .leftMouseUp, mouseCursorPosition: point, mouseButton: .left)
        else {
            throw HelperFailure(
                "DRIVER.ACTION_FAILED",
                "could not create pointer click events",
                data: progress.pointerMetadata(phase: "event_construction")
            )
        }
        move.setIntegerValueField(.mouseEventClickState, value: 1)
        down.setIntegerValueField(.mouseEventClickState, value: 1)
        up.setIntegerValueField(.mouseEventClickState, value: 1)
        try requireFrontmost(targetPID, operation: "pointer_click")
        try checkDeadline(deadlineMS)
        if !progress.pointerDispatchStarted {
            progress.markPointerDispatchStarted()
            emitPointerProgress(id: requestID, pointerDispatchStarted: true)
        }
        move.postToPid(targetPID)
        down.postToPid(targetPID)
        up.postToPid(targetPID)
        try checkDeadline(deadlineMS)
        return [
            "native_operation": "CGEventLeftClick",
            "submitted": true,
            "pointer_dispatch_started": progress.pointerDispatchStarted,
            "phase": "submitted",
        ]
    }

    private func setValue(args: Arguments, deadlineMS: Double?) throws -> [String: Any] {
        try args.only(["native_token", "value"])
        try requireTrusted()
        let value = try args.string("value")!
        let element = try tokens.resolve(try args.string("native_token")!)
        try configureTimeout(element, deadlineMS: deadlineMS)
        let role = stringAttribute(element, kAXRoleAttribute as CFString)
        let subrole = stringAttribute(element, kAXSubroleAttribute as CFString)
        if role == "AXSecureTextField" || subrole == "AXSecureTextField" {
            throw HelperFailure("DRIVER.PROTECTED_ELEMENT", "AXValue is protected")
        }
        guard isSettable(element, kAXValueAttribute as CFString) else {
            throw HelperFailure("DRIVER.ACTION_UNSUPPORTED", "AXValue is not settable", data: ["attribute": "AXValue"])
        }
        try checkDeadline(deadlineMS)
        let error = AXUIElementSetAttributeValue(
            element, kAXValueAttribute as CFString, value as CFString
        )
        try requireSuccess(error, operation: "AXUIElementSetAttributeValue(AXValue)")
        return ["native_operation": "AXValue", "accepted": true]
    }

    private func typeText(
        args: Arguments, deadlineMS: Double?, requestID: String,
        progress: RequestProgress
    ) throws -> [String: Any] {
        try args.only(["native_token", "text"])
        try requireTrusted()
        let text = try args.string("text")!
        guard !text.isEmpty, text.unicodeScalars.count <= maximumTypeTextScalars,
              text.utf16.count <= maximumTypeTextUTF16Units else {
            throw HelperFailure(
                "DRIVER.INVALID_REQUEST",
                "helper args.text exceeds the keyboard input bound"
            )
        }
        guard !text.unicodeScalars.contains(where: { CharacterSet.controlCharacters.contains($0) }) else {
            throw HelperFailure("DRIVER.INVALID_REQUEST", "helper args.text contains a control character")
        }
        let element = try tokens.resolve(try args.string("native_token")!)
        try configureTimeout(element, deadlineMS: deadlineMS)
        let role = stringAttribute(element, kAXRoleAttribute as CFString)
        let subrole = stringAttribute(element, kAXSubroleAttribute as CFString)
        if role == "AXSecureTextField" || subrole == "AXSecureTextField" {
            throw HelperFailure("DRIVER.PROTECTED_ELEMENT", "keyboard input target is protected")
        }
        guard let resolvedRole = role else {
            throw HelperFailure(
                "DRIVER.ACTION_UNSUPPORTED",
                "target is not an eligible text input role",
                data: ["role": NSNull()]
            )
        }
        guard isKeyboardTextTarget(role: resolvedRole) else {
            throw HelperFailure(
                "DRIVER.ACTION_UNSUPPORTED",
                "target is not an eligible text input role",
                data: ["role": resolvedRole]
            )
        }
        guard boolAttribute(element, kAXEnabledAttribute as CFString) != false else {
            throw HelperFailure(
                "DRIVER.ACTION_UNSUPPORTED", "keyboard input target is disabled",
                data: progress.keyboardMetadata(phase: "target_preflight")
            )
        }
        let secureEventInputEnabled = secureEventInputIsEnabled(
            IsSecureEventInputEnabled()
        )
        guard !secureEventInputEnabled else {
            throw HelperFailure(
                "DRIVER.PROTECTED_ELEMENT",
                "macOS Secure Event Input is enabled",
                data: progress.keyboardMetadata(phase: "secure_event_input_preflight")
            )
        }
        guard isSettable(element, kAXFocusedAttribute as CFString) else {
            throw HelperFailure(
                "DRIVER.ACTION_UNSUPPORTED",
                "AXFocused is not settable",
                data: [
                    "attribute": "AXFocused",
                    "phase": "focus_preflight",
                    "keyboard_dispatch_started": false,
                    "focus_changed": false,
                    "effect": "not_applied",
                ]
            )
        }
        var targetPID = pid_t(0)
        let pidError = AXUIElementGetPid(element, &targetPID)
        try requireSuccess(pidError, operation: "AXUIElementGetPid")
        guard targetPID > 0 else {
            throw HelperFailure("DRIVER.ACTION_FAILED", "keyboard input target has no valid process id")
        }
        try requireFrontmost(targetPID, operation: "type_text")

        try checkDeadline(deadlineMS)
        // From this marker until AX focus returns, the target's focus may have
        // changed, but no keyboard event has been submitted.
        progress.markFocusChanged()
        emitKeyboardFocusProgress(id: requestID, focusChanged: true)
        let focusError = AXUIElementSetAttributeValue(
            element, kAXFocusedAttribute as CFString, kCFBooleanTrue
        )
        try requireSuccess(focusError, operation: "AXUIElementSetAttributeValue(AXFocused)")
        guard boolAttribute(element, kAXFocusedAttribute as CFString) == true else {
            throw HelperFailure(
                "DRIVER.ACTION_FAILED", "AX target did not become focused",
                data: progress.keyboardMetadata(phase: "focus_verification")
            )
        }
        try requireFrontmost(targetPID, operation: "type_text")
        try checkDeadline(deadlineMS)

        let utf16 = Array(text.utf16)
        var offset = 0
        while offset < utf16.count {
            try requireFrontmost(targetPID, operation: "type_text")
            let secureEventInputEnabled = secureEventInputIsEnabled(
                IsSecureEventInputEnabled()
            )
            guard !secureEventInputEnabled else {
                throw HelperFailure(
                    "DRIVER.PROTECTED_ELEMENT",
                    "macOS Secure Event Input became enabled before keyboard dispatch",
                    data: progress.keyboardMetadata(phase: "secure_event_input_preflight")
                )
            }
            guard boolAttribute(element, kAXFocusedAttribute as CFString) == true else {
                throw HelperFailure(
                    "DRIVER.ACTION_FAILED",
                    "AX target lost focus during keyboard input"
                )
            }
            try checkDeadline(deadlineMS)
            var end = min(offset + maximumUnicodeUnitsPerEvent, utf16.count)
            if end < utf16.count, isHighSurrogate(utf16[end - 1]) { end -= 1 }
            guard end > offset else {
                throw HelperFailure("DRIVER.INVALID_REQUEST", "could not form a Unicode keyboard chunk")
            }
            let chunk = Array(utf16[offset..<end])
            try postUnicodeKeyboardChunk(
                chunk, targetPID: targetPID, requestID: requestID, progress: progress
            )
            offset = end
        }
        try checkDeadline(deadlineMS)
        return [
            "native_operation": "CGEventKeyboardSetUnicodeString",
            "submitted": true,
            "keyboard_dispatch_started": progress.keyboardDispatchStarted,
            "focus_changed": progress.focusChanged,
            "phase": "submitted",
        ]
    }

    private func requireFrontmost(_ targetPID: pid_t, operation: String) throws {
        guard NSWorkspace.shared.frontmostApplication?.processIdentifier == targetPID else {
            throw HelperFailure(
                "DRIVER.ACTION_FAILED",
                "\(operation) requires the selected application to remain frontmost",
                data: ["reason": "target_not_frontmost"]
            )
        }
    }

    private func isKeyboardTextTarget(role: String) -> Bool {
        ["AXTextField", "AXTextArea", "AXComboBox"].contains(role)
    }

    private func isHighSurrogate(_ value: UInt16) -> Bool {
        value >= 0xD800 && value <= 0xDBFF
    }

    private func postUnicodeKeyboardChunk(
        _ utf16: [UInt16], targetPID: pid_t, requestID: String,
        progress: RequestProgress
    ) throws {
        guard !utf16.isEmpty, utf16.count <= maximumUnicodeUnitsPerEvent else {
            throw HelperFailure("DRIVER.INVALID_REQUEST", "Unicode keyboard chunk is outside its bound")
        }
        guard let keyDown = CGEvent(keyboardEventSource: nil, virtualKey: 0, keyDown: true),
              let keyUp = CGEvent(keyboardEventSource: nil, virtualKey: 0, keyDown: false) else {
            throw HelperFailure("DRIVER.ACTION_FAILED", "could not create Unicode keyboard events")
        }
        utf16.withUnsafeBufferPointer { buffer in
            keyDown.keyboardSetUnicodeString(
                stringLength: buffer.count, unicodeString: buffer.baseAddress
            )
            keyUp.keyboardSetUnicodeString(
                stringLength: buffer.count, unicodeString: buffer.baseAddress
            )
        }
        let secureEventInputEnabled = secureEventInputIsEnabled(
            IsSecureEventInputEnabled()
        )
        guard !secureEventInputEnabled else {
            throw HelperFailure(
                "DRIVER.PROTECTED_ELEMENT",
                "macOS Secure Event Input became enabled before keyboard dispatch",
                data: progress.keyboardMetadata(phase: "secure_event_input_preflight")
            )
        }
        if !progress.keyboardDispatchStarted {
            progress.markKeyboardDispatchStarted()
            emitKeyboardDispatchProgress(
                id: requestID,
                keyboardDispatchStarted: true,
                focusChanged: progress.focusChanged
            )
        }
        keyDown.postToPid(targetPID)
        keyUp.postToPid(targetPID)
    }

    private func configureTimeout(_ element: AXUIElement, deadlineMS: Double?) throws {
        try checkDeadline(deadlineMS)
        let remainingSeconds: Double
        if let deadlineMS {
            remainingSeconds = max(0.001, (deadlineMS - Date().timeIntervalSince1970 * 1_000) / 1_000)
        } else {
            remainingSeconds = 2.0
        }
        let timeout = Float(min(2.0, remainingSeconds))
        let error = AXUIElementSetMessagingTimeout(element, timeout)
        guard error == .success else {
            throw HelperFailure(
                "DRIVER.ACTION_FAILED",
                "could not set AX messaging timeout",
                data: ["operation": "AXUIElementSetMessagingTimeout", "ax_error": Int(error.rawValue)]
            )
        }
    }

    private func requireSuccess(_ error: AXError, operation: String) throws {
        guard error == .success else {
            throw HelperFailure(
                "DRIVER.ACTION_FAILED",
                "native AX operation failed",
                data: ["operation": operation, "ax_error": Int(error.rawValue)]
            )
        }
    }

    private func copyAttribute(_ element: AXUIElement, _ name: CFString) -> CFTypeRef? {
        var raw: CFTypeRef?
        guard AXUIElementCopyAttributeValue(element, name, &raw) == .success else { return nil }
        return raw
    }

    private func stringAttribute(_ element: AXUIElement, _ name: CFString) -> String? {
        guard let raw = copyAttribute(element, name), CFGetTypeID(raw) == CFStringGetTypeID() else { return nil }
        return bounded(raw as! String)
    }

    private func scalarStringAttribute(_ element: AXUIElement, _ name: CFString) -> String? {
        guard let raw = copyAttribute(element, name) else { return nil }
        if CFGetTypeID(raw) == CFStringGetTypeID() { return bounded(raw as! String) }
        if CFGetTypeID(raw) == CFNumberGetTypeID() { return bounded((raw as! NSNumber).stringValue) }
        if CFGetTypeID(raw) == CFBooleanGetTypeID() { return (raw as! Bool) ? "true" : "false" }
        return nil
    }

    private func boolAttribute(_ element: AXUIElement, _ name: CFString) -> Bool? {
        guard let raw = copyAttribute(element, name), CFGetTypeID(raw) == CFBooleanGetTypeID() else { return nil }
        return raw as? Bool
    }

    private func isSettable(_ element: AXUIElement, _ name: CFString) -> Bool {
        var result = DarwinBoolean(false)
        return AXUIElementIsAttributeSettable(element, name, &result) == .success && result.boolValue
    }

    private func copyActionNames(_ element: AXUIElement) -> [String] {
        var raw: CFArray?
        guard AXUIElementCopyActionNames(element, &raw) == .success, let values = raw else { return [] }
        var result: [String] = []
        for index in 0..<CFArrayGetCount(values) {
            let rawPointer: UnsafeRawPointer? = CFArrayGetValueAtIndex(values, index)
            guard let pointer = rawPointer else { continue }
            let value = Unmanaged<CFTypeRef>.fromOpaque(pointer).takeUnretainedValue()
            if CFGetTypeID(value) == CFStringGetTypeID() {
                result.append(bounded(value as! String))
            }
        }
        return result
    }

    private func copyBounds(_ element: AXUIElement) -> [String: Int]? {
        guard
            let rawPosition = copyAttribute(element, kAXPositionAttribute as CFString),
            let rawSize = copyAttribute(element, kAXSizeAttribute as CFString),
            CFGetTypeID(rawPosition) == AXValueGetTypeID(),
            CFGetTypeID(rawSize) == AXValueGetTypeID()
        else { return nil }
        let positionValue = unsafeBitCast(rawPosition, to: AXValue.self)
        let sizeValue = unsafeBitCast(rawSize, to: AXValue.self)
        guard AXValueGetType(positionValue) == .cgPoint, AXValueGetType(sizeValue) == .cgSize else { return nil }
        var position = CGPoint.zero
        var size = CGSize.zero
        guard
            AXValueGetValue(positionValue, .cgPoint, &position),
            AXValueGetValue(sizeValue, .cgSize, &size)
        else { return nil }
        return [
            "x": finiteInt(position.x),
            "y": finiteInt(position.y),
            "width": max(0, finiteInt(size.width)),
            "height": max(0, finiteInt(size.height)),
        ]
    }
}

private func bounded(_ value: String) -> String {
    String(value.prefix(maximumFieldCharacters))
}

private func finiteInt(_ value: CGFloat) -> Int {
    guard value.isFinite else { return 0 }
    let rounded = value.rounded()
    if rounded >= CGFloat(Int.max) { return Int.max }
    if rounded <= CGFloat(Int.min) { return Int.min }
    return Int(rounded)
}

private func emit(_ value: [String: Any]) {
    guard JSONSerialization.isValidJSONObject(value), var data = try? JSONSerialization.data(withJSONObject: value) else {
        let fallback = "{\"id\":null,\"error\":{\"code\":\"DRIVER.ACTION_FAILED\",\"message\":\"helper could not encode response\",\"retryable\":false}}\n"
        FileHandle.standardOutput.write(Data(fallback.utf8))
        return
    }
    if data.count + 1 > maximumOutputBytes {
        let identifier: Any
        if let rawIdentifier = value["id"] as? String, rawIdentifier.count <= 256 {
            identifier = rawIdentifier
        } else {
            identifier = NSNull()
        }
        let fallback: [String: Any] = [
            "id": identifier,
            "error": [
                "code": "DRIVER.OUTPUT_TOO_LARGE",
                "message": "helper response exceeds its frame limit",
                "retryable": false,
                "data": ["limit_bytes": maximumOutputBytes],
            ],
        ]
        guard let bounded = try? JSONSerialization.data(withJSONObject: fallback),
              bounded.count + 1 <= maximumOutputBytes else {
            Darwin.exit(70)
        }
        data = bounded
    }
    FileHandle.standardOutput.write(data)
    FileHandle.standardOutput.write(Data([0x0A]))
}

private func emitFailure(_ failure: HelperFailure, id: Any) {
    var error: [String: Any] = [
        "code": failure.code,
        "message": failure.message,
        "retryable": failure.retryable,
    ]
    if !failure.data.isEmpty { error["data"] = failure.data }
    emit(["id": id, "error": error])
}

private func emitKeyboardFocusProgress(id: String, focusChanged: Bool) {
    emit([
        "id": id,
        "progress": [
            "phase": "focus_changed",
            "keyboard_dispatch_started": false,
            "focus_changed": focusChanged,
        ],
    ])
}

private func emitKeyboardDispatchProgress(
    id: String, keyboardDispatchStarted: Bool, focusChanged: Bool
) {
    emit([
        "id": id,
        "progress": [
            "phase": "keyboard_dispatch",
            "keyboard_dispatch_started": keyboardDispatchStarted,
            "focus_changed": focusChanged,
        ],
    ])
}

private func emitPointerProgress(id: String, pointerDispatchStarted: Bool) {
    emit([
        "id": id,
        "progress": [
            "phase": "pointer_dispatch",
            "pointer_dispatch_started": pointerDispatchStarted,
        ],
    ])
}

private func serve(_ service: AXService) {
    let input = FileHandle.standardInput
    var buffer = Data()
    var discardingOversizedFrame = false
    while true {
        let chunk = input.readData(ofLength: 65_536)
        if chunk.isEmpty { return }
        if discardingOversizedFrame {
            if let newline = chunk.firstIndex(of: 0x0A) {
                discardingOversizedFrame = false
                emitFailure(
                    HelperFailure("PROTOCOL.REQUEST_TOO_LARGE", "helper request exceeds its frame limit"),
                    id: NSNull()
                )
                buffer.append(contentsOf: chunk[chunk.index(after: newline)...])
            } else {
                continue
            }
        } else {
            buffer.append(chunk)
        }
        while let newline = buffer.firstIndex(of: 0x0A) {
            let frame = Data(buffer[..<newline])
            buffer.removeSubrange(...newline)
            if frame.count > maximumInputBytes {
                emitFailure(
                    HelperFailure("PROTOCOL.REQUEST_TOO_LARGE", "helper request exceeds its frame limit"),
                    id: NSNull()
                )
                continue
            }
            handleFrame(frame, service: service)
        }
        if buffer.count > maximumInputBytes {
            buffer.removeAll(keepingCapacity: true)
            discardingOversizedFrame = true
        }
    }
}

private func handleFrame(_ data: Data, service: AXService) {
    guard let object = try? JSONSerialization.jsonObject(with: data),
          let request = object as? [String: Any] else {
        emitFailure(HelperFailure("PROTOCOL.PARSE_ERROR", "helper request is not valid JSON"), id: NSNull())
        return
    }
    let requestID: Any = request["id"] ?? NSNull()
    do {
        let unknownFields = Set(request.keys).subtracting(["id", "operation", "args", "deadline_ms"])
        guard unknownFields.isEmpty else {
            throw HelperFailure(
                "PROTOCOL.INVALID_REQUEST",
                "helper request contains unsupported fields",
                data: ["fields": unknownFields.sorted()]
            )
        }
        guard let identifier = request["id"] as? String, !identifier.isEmpty, identifier.count <= 256 else {
            throw HelperFailure("PROTOCOL.INVALID_REQUEST", "helper request id is invalid")
        }
        guard let operation = request["operation"] as? String,
              !operation.isEmpty, operation.count <= 256 else {
            throw HelperFailure("PROTOCOL.INVALID_REQUEST", "helper operation is required")
        }
        guard let args = request["args"] as? [String: Any] else {
            throw HelperFailure("PROTOCOL.INVALID_REQUEST", "helper args must be an object")
        }
        guard let deadlineNumber = request["deadline_ms"] as? NSNumber,
              CFGetTypeID(deadlineNumber) != CFBooleanGetTypeID(),
              deadlineNumber.doubleValue.isFinite else {
            throw HelperFailure("PROTOCOL.INVALID_REQUEST", "helper deadline_ms must be finite numeric epoch milliseconds")
        }
        let deadlineMS = deadlineNumber.doubleValue
        let progress = RequestProgress()
        do {
            let result = try service.execute(
                operation: operation, args: args, deadlineMS: deadlineMS,
                requestID: identifier, progress: progress
            )
            emit(["id": identifier, "result": result])
        } catch let failure as HelperFailure {
            guard operation == "type_text" || operation == "pointer_click" else { throw failure }
            var data = failure.data
            let defaults = operation == "type_text"
                ? progress.keyboardMetadata(phase: data["phase"] as? String ?? "pre_dispatch")
                : progress.pointerMetadata(phase: data["phase"] as? String ?? "pre_dispatch")
            for (key, value) in defaults where data[key] == nil { data[key] = value }
            throw HelperFailure(
                failure.code, failure.message, retryable: failure.retryable, data: data
            )
        } catch {
            guard operation == "type_text" || operation == "pointer_click" else { throw error }
            let phase = progress.keyboardDispatchStarted
                ? "keyboard_dispatch"
                : progress.pointerDispatchStarted
                ? "pointer_dispatch"
                : "pre_dispatch"
            var data: [String: Any] = ["exception_type": String(describing: type(of: error))]
            if operation == "type_text" {
                data.merge(progress.keyboardMetadata(phase: phase)) { current, _ in current }
            } else {
                data.merge(progress.pointerMetadata(phase: phase)) { current, _ in current }
            }
            throw HelperFailure(
                "DRIVER.ACTION_FAILED",
                "unexpected native helper failure",
                data: data
            )
        }
    } catch let failure as HelperFailure {
        emitFailure(failure, id: requestID)
    } catch {
        emitFailure(
            HelperFailure(
                "DRIVER.ACTION_FAILED",
                "unexpected native helper failure",
                data: ["exception_type": String(describing: type(of: error))]
            ),
            id: requestID
        )
    }
}

serve(AXService())
