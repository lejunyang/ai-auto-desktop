import AppKit
import ApplicationServices
import Carbon.HIToolbox
import Darwin
import Foundation

/// 3 added capture, window_action, paste, clipboard_read and global_key.
private let protocolVersion = 3
private let maximumInputBytes = 1_048_576
private let maximumOutputBytes = 8 * 1_024 * 1_024 - 1
private let maximumFieldCharacters = 4_096
private let maximumTypeTextScalars = 1_024
private let maximumTypeTextUTF16Units = maximumTypeTextScalars * 2
private let maximumUnicodeUnitsPerEvent = 20
private let maximumDepth = 128
private let maximumNodes = 5_000
/// Operations whose failures report how far keyboard or pointer dispatch got.
private let keyboardOperations: Set<String> = ["type_text", "key_chord", "paste", "global_key"]
private let maximumPasteCharacters = 4_096
private let maximumClipboardCharacters = 65_536
private let maximumSavedPasteboardBytes = 32 * 1_024 * 1_024
private let maximumCapturePoints: CGFloat = 8_192
private let concealedType = NSPasteboard.PasteboardType("org.nspasteboard.ConcealedType")
private let transientType = NSPasteboard.PasteboardType("org.nspasteboard.TransientType")

/// Offers the pasted text lazily, so the helper sees whether anything asked for it.
private final class PasteProvider: NSObject, NSPasteboardItemDataProvider {
    let text: String
    private(set) var provided = false

    init(text: String) {
        self.text = text
    }

    func pasteboard(
        _ pasteboard: NSPasteboard?, item: NSPasteboardItem,
        provideDataForType type: NSPasteboard.PasteboardType
    ) {
        guard type == .string else { return }
        item.setString(text, forType: .string)
        provided = true
    }

    func pasteboardFinishedWithDataProvider(_ pasteboard: NSPasteboard) {}
}

/// CGWindowListCreateImage, looked up at run time: newer SDKs mark it
/// obsolete for new deployment targets, but it remains the API that captures
/// one window's own content without the windows covering it.
private typealias WindowImageFunction = @convention(c) (
    CGRect, UInt32, UInt32, UInt32
) -> Unmanaged<CGImage>?

private func createWindowImage(_ rect: CGRect, window: CGWindowID) -> CGImage? {
    guard let symbol = dlsym(UnsafeMutableRawPointer(bitPattern: -2), "CGWindowListCreateImage") else {
        return nil
    }
    let function = unsafeBitCast(symbol, to: WindowImageFunction.self)
    let includingWindow: UInt32 = 1 << 3  // kCGWindowListOptionIncludingWindow
    let options: UInt32 = (1 << 0) | (1 << 3)  // boundsIgnoreFraming | bestResolution
    return function(rect, includingWindow, window, options)?.takeRetainedValue()
}
private let pointerOperations: Set<String> = ["pointer_click", "pointer_gesture", "drag"]

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

    func fraction(_ name: String) throws -> Double {
        guard let number = values[name] as? NSNumber, CFGetTypeID(number) != CFBooleanGetTypeID(),
              number.doubleValue.isFinite, (0.0...1.0).contains(number.doubleValue) else {
            throw HelperFailure("DRIVER.INVALID_REQUEST", "helper args.\(name) must be a fraction in 0..1")
        }
        return number.doubleValue
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

// MARK: Watching a person work

/// Records what a person does in one application as raw events in the shape
/// `aad_control::recording` turns into steps. Read-only: the event tap only
/// listens, and nothing is posted.
private final class Recorder {
    let pid: pid_t
    let describe: (AXUIElement) -> [String: Any]
    private let lock = NSLock()
    private var events: [[String: Any]] = []
    private var dropped = 0
    private var focused: [String: Any]?
    private var observer: AXObserver?
    private(set) var tap: CFMachPort?
    private var runLoop: CFRunLoop?

    init(pid: pid_t, describe: @escaping (AXUIElement) -> [String: Any]) {
        self.pid = pid
        self.describe = describe
    }

    private func now() -> UInt64 { UInt64(ProcessInfo.processInfo.systemUptime * 1000) }

    private func push(_ event: [String: Any]) {
        lock.lock()
        defer { lock.unlock() }
        if events.count >= 2_000 {
            events.removeFirst()
            dropped += 1
        }
        events.append(event)
    }

    func drain() -> [String: Any] {
        lock.lock()
        defer { lock.unlock() }
        let taken = events
        let lost = dropped
        events = []
        dropped = 0
        return ["events": taken, "dropped": lost]
    }

    /// Install the observer and the tap on a thread of their own and run it.
    func start() throws {
        var created: AXObserver?
        let error = AXObserverCreate(pid, recorderAXCallback, &created)
        guard error == .success, let created else {
            throw HelperFailure("DRIVER.ACTION_FAILED", "cannot observe the application",
                                data: ["ax_error": Int(error.rawValue)])
        }
        observer = created
        let application = AXUIElementCreateApplication(pid)
        let context = Unmanaged.passUnretained(self).toOpaque()
        for notification in [kAXValueChangedNotification, kAXFocusedUIElementChangedNotification] {
            AXObserverAddNotification(created, application, notification as CFString, context)
        }
        let mask = (1 << CGEventType.leftMouseDown.rawValue)
            | (1 << CGEventType.rightMouseDown.rawValue)
            | (1 << CGEventType.otherMouseDown.rawValue)
            | (1 << CGEventType.scrollWheel.rawValue)
            | (1 << CGEventType.keyDown.rawValue)
        // Without Input Monitoring permission there is no tap; value changes
        // are still recorded, and the caller is told pointer input is not.
        tap = CGEvent.tapCreate(
            tap: .cgSessionEventTap, place: .headInsertEventTap, options: .listenOnly,
            eventsOfInterest: CGEventMask(mask), callback: recorderTapCallback, userInfo: context
        )
        let ready = DispatchSemaphore(value: 0)
        let thread = Thread { [self] in
            let loop = CFRunLoopGetCurrent()
            runLoop = loop
            CFRunLoopAddSource(loop, AXObserverGetRunLoopSource(created), .defaultMode)
            if let tap {
                let source = CFMachPortCreateRunLoopSource(kCFAllocatorDefault, tap, 0)
                CFRunLoopAddSource(loop, source, .defaultMode)
                CGEvent.tapEnable(tap: tap, enable: true)
            }
            ready.signal()
            CFRunLoopRun()
        }
        thread.name = "aad-recorder"
        thread.start()
        ready.wait()
    }

    func stop() {
        if let tap { CGEvent.tapEnable(tap: tap, enable: false) }
        if let runLoop { CFRunLoopStop(runLoop) }
        tap = nil
        observer = nil
    }

    func notified(element: AXUIElement, notification: String) {
        let node = describe(element)
        if notification == kAXFocusedUIElementChangedNotification as String {
            lock.lock()
            focused = node
            lock.unlock()
            return
        }
        let role = node["role"] as? String ?? ""
        if role == "AXCheckBox" || role == "AXRadioButton" || role == "AXSwitch" {
            let value = (copyValue(element) as? NSNumber)?.intValue
            push(["kind": "toggle", "checked": value == 1, "at_ms": now(), "node": node])
        } else if ["AXTextField", "AXTextArea", "AXComboBox", "AXSearchField", "AXSecureTextField"].contains(role) {
            push(["kind": "text", "at_ms": now(), "node": node])
        }
    }

    private func copyValue(_ element: AXUIElement) -> CFTypeRef? {
        var value: CFTypeRef?
        guard AXUIElementCopyAttributeValue(element, kAXValueAttribute as CFString, &value) == .success
        else { return nil }
        return value
    }

    /// The element under a point, if it belongs to the watched application.
    private func element(at point: CGPoint) -> AXUIElement? {
        var hit: AXUIElement?
        let error = AXUIElementCopyElementAtPosition(
            AXUIElementCreateSystemWide(), Float(point.x), Float(point.y), &hit
        )
        guard error == .success, let hit else { return nil }
        var owner = pid_t(0)
        guard AXUIElementGetPid(hit, &owner) == .success, owner == pid else { return nil }
        return hit
    }

    /// Names the control protocol uses for keys whose code is not a character.
    private static let namedKeys: [Int64: String] = [
        36: "enter", 48: "tab", 49: "space", 51: "backspace", 53: "escape", 117: "delete",
        115: "home", 119: "end", 116: "pageup", 121: "pagedown", 123: "left", 124: "right",
        125: "down", 126: "up", 122: "f1", 120: "f2", 99: "f3", 118: "f4", 96: "f5", 97: "f6",
        98: "f7", 100: "f8", 101: "f9", 109: "f10", 103: "f11", 111: "f12",
    ]
    private static let characterKeys: [Int64: String] = [
        0: "a", 1: "s", 2: "d", 3: "f", 4: "h", 5: "g", 6: "z", 7: "x", 8: "c", 9: "v",
        11: "b", 12: "q", 13: "w", 14: "e", 15: "r", 16: "y", 17: "t", 18: "1", 19: "2",
        20: "3", 21: "4", 22: "6", 23: "5", 25: "9", 26: "7", 28: "8", 29: "0", 31: "o",
        32: "u", 34: "i", 35: "p", 37: "l", 38: "j", 40: "k", 45: "n", 46: "m",
    ]

    func tapped(type: CGEventType, event: CGEvent) {
        if type == .tapDisabledByTimeout || type == .tapDisabledByUserInput {
            if let tap { CGEvent.tapEnable(tap: tap, enable: true) }
            return
        }
        switch type {
        case .leftMouseDown, .rightMouseDown, .otherMouseDown:
            guard let element = element(at: event.location) else { return }
            let button = type == .leftMouseDown ? "left" : type == .rightMouseDown ? "right" : "middle"
            push(["kind": "click", "button": button, "at_ms": now(), "node": describe(element)])
        case .scrollWheel:
            guard let element = element(at: event.location) else { return }
            let vertical = event.getIntegerValueField(.scrollWheelEventDeltaAxis1)
            let horizontal = event.getIntegerValueField(.scrollWheelEventDeltaAxis2)
            let direction = vertical > 0 ? "up" : vertical < 0 ? "down"
                : horizontal > 0 ? "left" : horizontal < 0 ? "right" : ""
            guard !direction.isEmpty else { return }
            push(["kind": "wheel", "direction": direction, "at_ms": now(), "node": describe(element)])
        case .keyDown:
            guard NSWorkspace.shared.frontmostApplication?.processIdentifier == pid else { return }
            let code = event.getIntegerValueField(.keyboardEventKeycode)
            let flags = event.flags
            let named = Self.namedKeys[code]
            guard let key = named ?? Self.characterKeys[code] else { return }
            let held = flags.contains(.maskCommand) || flags.contains(.maskControl)
                || flags.contains(.maskAlternate)
            // Plain typing is recorded as the text it made.
            guard held || ["enter", "tab", "escape"].contains(key) || key.hasPrefix("f") && key.count > 1
            else { return }
            var keys: [String] = []
            if flags.contains(.maskCommand) { keys.append("cmd") }
            if flags.contains(.maskControl) { keys.append("ctrl") }
            if flags.contains(.maskShift) { keys.append("shift") }
            if flags.contains(.maskAlternate) { keys.append("alt") }
            keys.append(key)
            lock.lock()
            let node = focused
            lock.unlock()
            push(["kind": "key", "keys": keys, "at_ms": now(), "node": node ?? NSNull()])
        default:
            return
        }
    }
}

private func recorderAXCallback(
    _ observer: AXObserver, _ element: AXUIElement, _ notification: CFString,
    _ refcon: UnsafeMutableRawPointer?
) {
    guard let refcon else { return }
    Unmanaged<Recorder>.fromOpaque(refcon).takeUnretainedValue()
        .notified(element: element, notification: notification as String)
}

private func recorderTapCallback(
    _ proxy: CGEventTapProxy, _ type: CGEventType, _ event: CGEvent,
    _ refcon: UnsafeMutableRawPointer?
) -> Unmanaged<CGEvent>? {
    if let refcon {
        Unmanaged<Recorder>.fromOpaque(refcon).takeUnretainedValue().tapped(type: type, event: event)
    }
    return Unmanaged.passUnretained(event)
}

private final class AXService {
    private let tokens = TokenStore()
    private var recorders: [String: Recorder] = [:]

    /// An element as recorded events describe it, matching how a snapshot
    /// names it: the raw AX role, and the title or else the description.
    private func recordedNode(_ element: AXUIElement) -> [String: Any] {
        let role = stringAttribute(element, kAXRoleAttribute as CFString) ?? ""
        let subrole = stringAttribute(element, kAXSubroleAttribute as CFString)
        let protected = role == "AXSecureTextField" || subrole == "AXSecureTextField"
        let name = stringAttribute(element, kAXTitleAttribute as CFString)
            ?? stringAttribute(element, kAXDescriptionAttribute as CFString)
        var node: [String: Any] = [
            "role": bounded(role), "name": name.map(bounded) ?? "", "protected": protected,
        ]
        if !protected, let value = scalarStringAttribute(element, kAXValueAttribute as CFString) {
            node["value"] = value
        }
        return node
    }

    private func recording(_ arguments: Arguments) throws -> (String, Recorder) {
        try arguments.only(["recording_id"])
        let id = try arguments.string("recording_id")!
        guard let recorder = recorders[id] else {
            throw HelperFailure("DRIVER.NOT_FOUND", "no such recording")
        }
        return (id, recorder)
    }

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
        case "pointer_gesture":
            return try pointerGesture(
                args: Arguments(values: args), deadlineMS: deadlineMS,
                requestID: requestID, progress: progress
            )
        case "drag":
            return try drag(
                args: Arguments(values: args), deadlineMS: deadlineMS,
                requestID: requestID, progress: progress
            )
        case "key_chord":
            return try keyChord(
                args: Arguments(values: args), deadlineMS: deadlineMS,
                requestID: requestID, progress: progress
            )
        case "record_start":
            let arguments = Arguments(values: args)
            try arguments.only(["process_id"])
            try requireTrusted()
            let pid = pid_t(try arguments.integer("process_id", minimum: 1, maximum: Int(Int32.max)))
            let recorder = Recorder(pid: pid) { [unowned self] element in self.recordedNode(element) }
            try recorder.start()
            let id = UUID().uuidString
            recorders[id] = recorder
            return ["recording_id": id, "pointer_watch": recorder.tap != nil]
        case "record_collect":
            return try recording(Arguments(values: args)).1.drain()
        case "record_stop":
            let (id, recorder) = try recording(Arguments(values: args))
            recorder.stop()
            recorders.removeValue(forKey: id)
            return ["stopped": true]
        case "capture":
            return try captureElement(args: Arguments(values: args), deadlineMS: deadlineMS)
        case "window_action":
            return try windowAction(args: Arguments(values: args), deadlineMS: deadlineMS)
        case "paste":
            return try paste(
                args: Arguments(values: args), deadlineMS: deadlineMS,
                requestID: requestID, progress: progress
            )
        case "clipboard_read":
            return try clipboardRead(args: Arguments(values: args))
        case "global_key":
            return try globalKey(
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
            // The executable's file name, as desktop launch matches a program it started.
            if let executable = app.executableURL?.lastPathComponent {
                result["process_name"] = bounded(executable)
            }
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
            actions.append(contentsOf: ["pointer_click", "pointer_gesture", "drag"])
        }
        if focusSettable && enabled != false && !protected { actions.append("key_chord") }
        if actionNames.contains(kAXPressAction as String) && enabled != false { actions.append("invoke") }
        if valueSettable && !protected && enabled != false { actions.append("set_value") }
        if isKeyboardTextTarget(role: role) && focusSettable && !protected && enabled != false {
            actions.append("type_text")
        }
        if focusSettable && valueSettable && !protected && enabled != false {
            actions.append("paste")
        }
        if role == "AXWindow" {
            actions.append(contentsOf: windowActions(element))
        }
        var provenance: [String: Any] = [
            "ax_role": role,
            "process_id": app["process_id"] ?? NSNull(),
            "coordinate_space": "screen_points",
            "value_redacted": protected,
        ]
        if role == "AXWindow" {
            provenance["window"] = windowState(element)
        }
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

    // MARK: Gestures, drags and key chords

    private struct PointerTarget {
        let pid: pid_t
        let point: CGPoint
    }

    /// A point inside an enabled, unprotected element of the frontmost
    /// application, confirmed by AX hit testing to land on that element or
    /// one of its descendants.
    private func pointerTarget(
        token: String, fx: Double, fy: Double, operation: String,
        deadlineMS: Double?, progress: RequestProgress
    ) throws -> PointerTarget {
        let element = try tokens.resolve(token)
        try configureTimeout(element, deadlineMS: deadlineMS)
        let role = stringAttribute(element, kAXRoleAttribute as CFString)
        let subrole = stringAttribute(element, kAXSubroleAttribute as CFString)
        guard role != "AXSecureTextField", subrole != "AXSecureTextField" else {
            throw HelperFailure(
                "DRIVER.PROTECTED_ELEMENT",
                "protected element does not permit \(operation)",
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
        guard let bounds = copyBounds(element),
              bounds["width", default: 0] > 0, bounds["height", default: 0] > 0 else {
            throw HelperFailure(
                "DRIVER.ACTION_UNSUPPORTED",
                "pointer target has no positive-area bounds",
                data: progress.pointerMetadata(phase: "bounds_preflight")
            )
        }
        var targetPID = pid_t(0)
        try requireSuccess(AXUIElementGetPid(element, &targetPID), operation: "AXUIElementGetPid")
        guard targetPID > 0 else {
            throw HelperFailure("DRIVER.ACTION_FAILED", "pointer target has no valid process id")
        }
        try requireFrontmost(targetPID, operation: operation)
        // One point in from each edge, so the hit lands inside the element.
        func along(_ start: Int, _ size: Int, _ fraction: Double) -> CGFloat {
            let extent = Double(size)
            let offset = extent < 3 ? extent / 2 : min(max(extent * fraction, 1), extent - 1)
            return CGFloat(Double(start) + offset)
        }
        let point = CGPoint(
            x: along(bounds["x", default: 0], bounds["width", default: 0], fx),
            y: along(bounds["y", default: 0], bounds["height", default: 0], fy)
        )
        let application = AXUIElementCreateApplication(targetPID)
        try configureTimeout(application, deadlineMS: deadlineMS)
        var hitElement: AXUIElement?
        let hitError = AXUIElementCopyElementAtPosition(
            application, Float(point.x), Float(point.y), &hitElement
        )
        try requireSuccess(hitError, operation: "AXUIElementCopyElementAtPosition")
        guard let hitElement, isSelfOrDescendant(hitElement, of: element) else {
            throw HelperFailure(
                "DRIVER.ACTION_FAILED",
                "pointer hit test does not land on the target element",
                data: progress.pointerMetadata(phase: "hit_test_verification")
            )
        }
        return PointerTarget(pid: targetPID, point: point)
    }

    private func isSelfOrDescendant(_ candidate: AXUIElement, of ancestor: AXUIElement) -> Bool {
        var current = candidate
        for _ in 0..<maximumDepth {
            if CFEqual(current, ancestor) { return true }
            var parent: CFTypeRef?
            let error = AXUIElementCopyAttributeValue(current, kAXParentAttribute as CFString, &parent)
            guard error == .success, let parent, CFGetTypeID(parent) == AXUIElementGetTypeID() else {
                return false
            }
            current = unsafeBitCast(parent, to: AXUIElement.self)
        }
        return false
    }

    private func mouseTypes(_ button: String) throws -> (
        down: CGEventType, up: CGEventType, dragged: CGEventType, button: CGMouseButton
    ) {
        switch button {
        case "left": return (.leftMouseDown, .leftMouseUp, .leftMouseDragged, .left)
        case "right": return (.rightMouseDown, .rightMouseUp, .rightMouseDragged, .right)
        case "middle": return (.otherMouseDown, .otherMouseUp, .otherMouseDragged, .center)
        default:
            throw HelperFailure(
                "DRIVER.INVALID_REQUEST", "helper args.button must be left, right or middle"
            )
        }
    }

    private func mouseEvent(
        _ type: CGEventType, at point: CGPoint, button: CGMouseButton, clicks: Int64 = 1
    ) throws -> CGEvent {
        guard let event = CGEvent(
            mouseEventSource: nil, mouseType: type, mouseCursorPosition: point, mouseButton: button
        ) else {
            throw HelperFailure("DRIVER.ACTION_FAILED", "could not create pointer events")
        }
        event.setIntegerValueField(.mouseEventClickState, value: clicks)
        return event
    }

    private func startPointerDispatch(requestID: String, progress: RequestProgress) {
        if !progress.pointerDispatchStarted {
            progress.markPointerDispatchStarted()
            emitPointerProgress(id: requestID, pointerDispatchStarted: true)
        }
    }

    /// Rest for a while, but never past the request's deadline.
    private func dwell(milliseconds: Int, deadlineMS: Double?) {
        var until = Date().addingTimeInterval(Double(milliseconds) / 1000)
        if let deadlineMS {
            until = min(until, Date(timeIntervalSince1970: deadlineMS / 1000))
        }
        while Date() < until { usleep(10_000) }
    }

    private func pointerGesture(
        args: Arguments, deadlineMS: Double?, requestID: String,
        progress: RequestProgress
    ) throws -> [String: Any] {
        try args.only([
            "native_token", "mode", "button", "count", "direction", "notches", "hover_ms", "fx", "fy",
        ])
        try requireTrusted()
        let mode = try args.string("mode")!
        let target = try pointerTarget(
            token: try args.string("native_token")!,
            fx: try args.fraction("fx"), fy: try args.fraction("fy"),
            operation: "pointer_gesture", deadlineMS: deadlineMS, progress: progress
        )
        var events = [try mouseEvent(.mouseMoved, at: target.point, button: .left)]
        var dwellMS = 0
        switch mode {
        case "click":
            try args.only(["native_token", "mode", "button", "count", "fx", "fy"])
            let types = try mouseTypes(try args.string("button")!)
            let count = try args.integer("count", minimum: 1, maximum: 2)
            for click in 1...count {
                events.append(try mouseEvent(
                    types.down, at: target.point, button: types.button, clicks: Int64(click)
                ))
                events.append(try mouseEvent(
                    types.up, at: target.point, button: types.button, clicks: Int64(click)
                ))
            }
        case "wheel":
            try args.only(["native_token", "mode", "direction", "notches", "fx", "fy"])
            let notches = Int32(try args.integer("notches", minimum: 1, maximum: 30))
            // Positive wheel1 scrolls up, positive wheel2 scrolls left.
            let delta: (vertical: Int32, horizontal: Int32)
            switch try args.string("direction")! {
            case "up": delta = (notches, 0)
            case "down": delta = (-notches, 0)
            case "left": delta = (0, notches)
            case "right": delta = (0, -notches)
            default:
                throw HelperFailure(
                    "DRIVER.INVALID_REQUEST",
                    "helper args.direction must be up, down, left or right"
                )
            }
            guard let wheel = CGEvent(
                scrollWheelEvent2Source: nil, units: .line, wheelCount: 2,
                wheel1: delta.vertical, wheel2: delta.horizontal, wheel3: 0
            ) else {
                throw HelperFailure("DRIVER.ACTION_FAILED", "could not create a scroll event")
            }
            wheel.location = target.point
            events.append(wheel)
        case "hover":
            try args.only(["native_token", "mode", "hover_ms", "fx", "fy"])
            dwellMS = try args.integer("hover_ms", minimum: 1, maximum: 5_000)
        default:
            throw HelperFailure("DRIVER.INVALID_REQUEST", "helper args.mode must be click, wheel or hover")
        }
        try requireFrontmost(target.pid, operation: "pointer_gesture")
        try checkDeadline(deadlineMS)
        startPointerDispatch(requestID: requestID, progress: progress)
        for event in events { event.postToPid(target.pid) }
        if dwellMS > 0 { dwell(milliseconds: dwellMS, deadlineMS: deadlineMS) }
        return [
            "native_operation": "CGEventPointerGesture",
            "submitted": true,
            "pointer_dispatch_started": progress.pointerDispatchStarted,
            "phase": "submitted",
        ]
    }

    private func drag(
        args: Arguments, deadlineMS: Double?, requestID: String,
        progress: RequestProgress
    ) throws -> [String: Any] {
        try args.only(["native_token", "to_token", "button", "fx", "fy", "to_fx", "to_fy"])
        try requireTrusted()
        let types = try mouseTypes(try args.string("button")!)
        let from = try pointerTarget(
            token: try args.string("native_token")!,
            fx: try args.fraction("fx"), fy: try args.fraction("fy"),
            operation: "drag", deadlineMS: deadlineMS, progress: progress
        )
        let to = try pointerTarget(
            token: try args.string("to_token")!,
            fx: try args.fraction("to_fx"), fy: try args.fraction("to_fy"),
            operation: "drag", deadlineMS: deadlineMS, progress: progress
        )
        guard from.pid == to.pid else {
            throw HelperFailure("DRIVER.INVALID_REQUEST", "a drag must stay within one application")
        }
        var events = [
            try mouseEvent(.mouseMoved, at: from.point, button: types.button),
            try mouseEvent(types.down, at: from.point, button: types.button),
        ]
        // Glide in steps: many controls only begin a drag after the pointer
        // has moved a few points with the button held.
        let steps = 12
        for step in 1...steps {
            let progressAlong = CGFloat(step) / CGFloat(steps)
            let point = CGPoint(
                x: from.point.x + (to.point.x - from.point.x) * progressAlong,
                y: from.point.y + (to.point.y - from.point.y) * progressAlong
            )
            events.append(try mouseEvent(types.dragged, at: point, button: types.button))
        }
        events.append(try mouseEvent(types.up, at: to.point, button: types.button))
        try requireFrontmost(from.pid, operation: "drag")
        try checkDeadline(deadlineMS)
        startPointerDispatch(requestID: requestID, progress: progress)
        for (index, event) in events.enumerated() {
            event.postToPid(from.pid)
            if index >= 1 { usleep(15_000) }
        }
        return [
            "native_operation": "CGEventDrag",
            "submitted": true,
            "pointer_dispatch_started": progress.pointerDispatchStarted,
            "phase": "submitted",
        ]
    }

    /// US ANSI virtual key codes for the key names the control protocol uses.
    private static let keyCodes: [String: CGKeyCode] = [
        "a": 0, "s": 1, "d": 2, "f": 3, "h": 4, "g": 5, "z": 6, "x": 7, "c": 8, "v": 9,
        "b": 11, "q": 12, "w": 13, "e": 14, "r": 15, "y": 16, "t": 17, "1": 18, "2": 19,
        "3": 20, "4": 21, "6": 22, "5": 23, "9": 25, "7": 26, "8": 28, "0": 29, "o": 31,
        "u": 32, "i": 34, "p": 35, "l": 37, "j": 38, "k": 40, "n": 45, "m": 46,
        "enter": 36, "return": 36, "tab": 48, "space": 49, "backspace": 51, "escape": 53,
        "esc": 53, "delete": 117, "home": 115, "end": 119, "pageup": 116, "pagedown": 121,
        "left": 123, "right": 124, "down": 125, "up": 126,
        "f1": 122, "f2": 120, "f3": 99, "f4": 118, "f5": 96, "f6": 97, "f7": 98, "f8": 100,
        "f9": 101, "f10": 109, "f11": 103, "f12": 111, "f13": 105, "f14": 107, "f15": 113,
        "f16": 106, "f17": 64, "f18": 79, "f19": 80, "f20": 90,
    ]

    /// `ctrl` is Control and `cmd` (or `meta`, `win`, `super`) is Command:
    /// names are taken literally, never translated between platforms.
    private static let modifierKeys: [String: (code: CGKeyCode, flag: CGEventFlags)] = [
        "ctrl": (59, .maskControl), "control": (59, .maskControl),
        "shift": (56, .maskShift),
        "alt": (58, .maskAlternate), "option": (58, .maskAlternate),
        "cmd": (55, .maskCommand), "command": (55, .maskCommand), "meta": (55, .maskCommand),
        "win": (55, .maskCommand), "super": (55, .maskCommand),
    ]

    private func keyEvent(_ code: CGKeyCode, down: Bool, flags: CGEventFlags) throws -> CGEvent {
        guard let event = CGEvent(keyboardEventSource: nil, virtualKey: code, keyDown: down) else {
            throw HelperFailure("DRIVER.ACTION_FAILED", "could not create key events")
        }
        event.flags = flags
        return event
    }

    private func keyChord(
        args: Arguments, deadlineMS: Double?, requestID: String,
        progress: RequestProgress
    ) throws -> [String: Any] {
        try args.only(["native_token", "keys"])
        try requireTrusted()
        guard let raw = args.values["keys"] as? [String], (1...6).contains(raw.count) else {
            throw HelperFailure("DRIVER.INVALID_REQUEST", "helper args.keys must hold 1..6 names")
        }
        let names = raw.map { $0.lowercased() }
        var modifiers: [(code: CGKeyCode, flag: CGEventFlags)] = []
        for name in names.dropLast() {
            guard let modifier = Self.modifierKeys[name] else {
                throw HelperFailure(
                    "DRIVER.INVALID_REQUEST", "only modifiers may precede the main key",
                    data: ["key": name]
                )
            }
            modifiers.append(modifier)
        }
        guard let main = Self.keyCodes[names[names.count - 1]] else {
            throw HelperFailure(
                "DRIVER.ACTION_UNSUPPORTED", "the main key has no macOS key code here",
                data: ["key": names[names.count - 1]]
            )
        }
        let element = try tokens.resolve(try args.string("native_token")!)
        try configureTimeout(element, deadlineMS: deadlineMS)
        let role = stringAttribute(element, kAXRoleAttribute as CFString)
        let subrole = stringAttribute(element, kAXSubroleAttribute as CFString)
        if role == "AXSecureTextField" || subrole == "AXSecureTextField" {
            throw HelperFailure("DRIVER.PROTECTED_ELEMENT", "keyboard input target is protected")
        }
        guard boolAttribute(element, kAXEnabledAttribute as CFString) != false else {
            throw HelperFailure(
                "DRIVER.ACTION_UNSUPPORTED", "keyboard input target is disabled",
                data: progress.keyboardMetadata(phase: "target_preflight")
            )
        }
        guard !secureEventInputIsEnabled(IsSecureEventInputEnabled()) else {
            throw HelperFailure(
                "DRIVER.PROTECTED_ELEMENT", "macOS Secure Event Input is enabled",
                data: progress.keyboardMetadata(phase: "secure_event_input_preflight")
            )
        }
        guard isSettable(element, kAXFocusedAttribute as CFString) else {
            throw HelperFailure(
                "DRIVER.ACTION_UNSUPPORTED", "AXFocused is not settable",
                data: progress.keyboardMetadata(phase: "focus_preflight")
            )
        }
        var targetPID = pid_t(0)
        try requireSuccess(AXUIElementGetPid(element, &targetPID), operation: "AXUIElementGetPid")
        guard targetPID > 0 else {
            throw HelperFailure("DRIVER.ACTION_FAILED", "keyboard input target has no valid process id")
        }
        try requireFrontmost(targetPID, operation: "key_chord")
        try checkDeadline(deadlineMS)
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
        var flags = CGEventFlags()
        var events: [CGEvent] = []
        for modifier in modifiers {
            flags.insert(modifier.flag)
            events.append(try keyEvent(modifier.code, down: true, flags: flags))
        }
        events.append(try keyEvent(main, down: true, flags: flags))
        events.append(try keyEvent(main, down: false, flags: flags))
        for modifier in modifiers.reversed() {
            flags.remove(modifier.flag)
            events.append(try keyEvent(modifier.code, down: false, flags: flags))
        }
        try requireFrontmost(targetPID, operation: "key_chord")
        guard !secureEventInputIsEnabled(IsSecureEventInputEnabled()) else {
            throw HelperFailure(
                "DRIVER.PROTECTED_ELEMENT",
                "macOS Secure Event Input became enabled before keyboard dispatch",
                data: progress.keyboardMetadata(phase: "secure_event_input_preflight")
            )
        }
        try checkDeadline(deadlineMS)
        progress.markKeyboardDispatchStarted()
        emitKeyboardDispatchProgress(
            id: requestID, keyboardDispatchStarted: true, focusChanged: progress.focusChanged
        )
        for event in events { event.postToPid(targetPID) }
        return [
            "native_operation": "CGEventKeyChord",
            "submitted": true,
            "keyboard_dispatch_started": progress.keyboardDispatchStarted,
            "focus_changed": progress.focusChanged,
            "phase": "submitted",
        ]
    }

    // MARK: windows, capture, pasteboard and global keys

    /// Poll (bounded by 3 seconds and the request deadline), letting the run
    /// loop deliver pasteboard requests and window notifications meanwhile.
    private func waitUntil(_ deadlineMS: Double?, _ condition: () -> Bool) -> Bool {
        let limit = Date().addingTimeInterval(3)
        while true {
            if condition() { return true }
            let now = Date()
            if now >= limit { return false }
            if let deadlineMS, now.timeIntervalSince1970 * 1_000 >= deadlineMS { return false }
            RunLoop.current.run(until: now.addingTimeInterval(0.05))
        }
    }

    private func elementAttribute(_ element: AXUIElement, _ name: CFString) -> AXUIElement? {
        guard let raw = copyAttribute(element, name), CFGetTypeID(raw) == AXUIElementGetTypeID() else {
            return nil
        }
        return unsafeBitCast(raw, to: AXUIElement.self)
    }

    private func elementList(_ element: AXUIElement, _ name: CFString) -> [AXUIElement] {
        guard let raw = copyAttribute(element, name), CFGetTypeID(raw) == CFArrayGetTypeID() else {
            return []
        }
        let values = unsafeBitCast(raw, to: CFArray.self)
        var result: [AXUIElement] = []
        for index in 0..<CFArrayGetCount(values) {
            guard let pointer = CFArrayGetValueAtIndex(values, index) else { continue }
            let value = Unmanaged<CFTypeRef>.fromOpaque(pointer).takeUnretainedValue()
            if CFGetTypeID(value) == AXUIElementGetTypeID() {
                result.append(unsafeBitCast(value, to: AXUIElement.self))
            }
        }
        return result
    }

    private func closeButton(_ window: AXUIElement) -> AXUIElement? {
        guard let button = elementAttribute(window, kAXCloseButtonAttribute as CFString),
              copyActionNames(button).contains(kAXPressAction as String) else { return nil }
        return button
    }

    private func isActiveWindow(_ window: AXUIElement) -> Bool {
        var pid = pid_t(0)
        guard AXUIElementGetPid(window, &pid) == .success,
              NSWorkspace.shared.frontmostApplication?.processIdentifier == pid,
              let main = elementAttribute(AXUIElementCreateApplication(pid), kAXMainWindowAttribute as CFString)
        else { return false }
        return CFEqual(main, window)
    }

    private func windowState(_ window: AXUIElement) -> [String: Any] {
        [
            "minimized": boolAttribute(window, kAXMinimizedAttribute as CFString) ?? false,
            "full_screen": boolAttribute(window, "AXFullScreen" as CFString) ?? false,
            "active": isActiveWindow(window),
        ]
    }

    /// The window actions an AXWindow offers in its current state.
    private func windowActions(_ window: AXUIElement) -> [String] {
        let minimizedName = kAXMinimizedAttribute as CFString
        let fullScreenName = "AXFullScreen" as CFString
        let minimized = boolAttribute(window, minimizedName) == true
        let fullScreen = boolAttribute(window, fullScreenName) == true
        let canMinimize = isSettable(window, minimizedName)
        let canFullScreen = isSettable(window, fullScreenName)
        var actions: [String] = []
        if canMinimize && !minimized { actions.append("minimize") }
        if canFullScreen && !fullScreen && !minimized { actions.append("maximize") }
        if (minimized && canMinimize) || (fullScreen && canFullScreen) { actions.append("restore") }
        if closeButton(window) != nil { actions.append("close_window") }
        if !minimized && !isActiveWindow(window) { actions.append("activate") }
        return actions
    }

    private func setBool(_ element: AXUIElement, _ name: CFString, _ value: Bool) throws {
        guard isSettable(element, name) else {
            throw HelperFailure(
                "DRIVER.ACTION_UNSUPPORTED", "window attribute is not settable",
                data: ["attribute": name as String]
            )
        }
        let error = AXUIElementSetAttributeValue(element, name, NSNumber(value: value))
        try requireSuccess(error, operation: "AXUIElementSetAttributeValue(\(name as String))")
    }

    private func windowStillOpen(_ window: AXUIElement, pid: pid_t) -> Bool {
        guard let app = NSRunningApplication(processIdentifier: pid), !app.isTerminated else {
            return false
        }
        return elementList(AXUIElementCreateApplication(pid), kAXWindowsAttribute as CFString)
            .contains { CFEqual($0, window) }
    }

    private func windowAction(args: Arguments, deadlineMS: Double?) throws -> [String: Any] {
        try args.only(["native_token", "action"])
        try requireTrusted()
        let action = try args.string("action")!
        let window = try tokens.resolve(try args.string("native_token")!)
        try configureTimeout(window, deadlineMS: deadlineMS)
        guard stringAttribute(window, kAXRoleAttribute as CFString) == "AXWindow" else {
            throw HelperFailure("DRIVER.ACTION_UNSUPPORTED", "window actions need an AXWindow")
        }
        var pid = pid_t(0)
        try requireSuccess(AXUIElementGetPid(window, &pid), operation: "AXUIElementGetPid")
        let minimizedName = kAXMinimizedAttribute as CFString
        let fullScreenName = "AXFullScreen" as CFString
        try checkDeadline(deadlineMS)
        let verified: Bool
        switch action {
        case "minimize":
            try setBool(window, minimizedName, true)
            verified = waitUntil(deadlineMS) { boolAttribute(window, minimizedName) == true }
        case "maximize":
            try setBool(window, fullScreenName, true)
            verified = waitUntil(deadlineMS) { boolAttribute(window, fullScreenName) == true }
        case "restore":
            if boolAttribute(window, minimizedName) == true {
                try setBool(window, minimizedName, false)
                verified = waitUntil(deadlineMS) { boolAttribute(window, minimizedName) == false }
            } else if boolAttribute(window, fullScreenName) == true {
                try setBool(window, fullScreenName, false)
                verified = waitUntil(deadlineMS) { boolAttribute(window, fullScreenName) == false }
            } else {
                throw HelperFailure(
                    "DRIVER.ACTION_UNSUPPORTED", "the window is neither minimized nor full screen"
                )
            }
        case "close_window":
            guard let button = closeButton(window) else {
                throw HelperFailure("DRIVER.ACTION_UNSUPPORTED", "the window has no close button")
            }
            let error = AXUIElementPerformAction(button, kAXPressAction as CFString)
            try requireSuccess(error, operation: "AXUIElementPerformAction(AXCloseButton.AXPress)")
            verified = waitUntil(deadlineMS) { !windowStillOpen(window, pid: pid) }
            if verified {
                return [
                    "native_operation": "AXCloseButton.AXPress", "accepted": true,
                    "verified": true, "observed_state": ["exists": false],
                ]
            }
        case "activate":
            guard let app = NSRunningApplication(processIdentifier: pid) else {
                throw HelperFailure("DRIVER.STALE_SNAPSHOT", "the window's application is gone")
            }
            _ = app.activate(options: [])
            _ = AXUIElementPerformAction(window, kAXRaiseAction as CFString)
            verified = waitUntil(deadlineMS) { isActiveWindow(window) }
        default:
            throw HelperFailure("DRIVER.INVALID_REQUEST", "unknown window action", data: ["action": action])
        }
        return [
            "native_operation": "AXWindow.\(action)", "accepted": true,
            "verified": verified, "observed_state": windowState(window),
        ]
    }

    /// The on-screen CGWindow of `pid` whose frame is the AX window's frame.
    private func cgWindowID(pid: pid_t, frame: CGRect) -> CGWindowID? {
        guard let raw = CGWindowListCopyWindowInfo(
            [.optionOnScreenOnly, .excludeDesktopElements], kCGNullWindowID
        ) as? [[String: Any]] else { return nil }
        let matches = raw.filter { info in
            guard (info[kCGWindowOwnerPID as String] as? Int) == Int(pid),
                  (info[kCGWindowLayer as String] as? Int) == 0,
                  let bounds = info[kCGWindowBounds as String] as? NSDictionary,
                  let rect = CGRect(dictionaryRepresentation: bounds as CFDictionary)
            else { return false }
            return abs(rect.minX - frame.minX) <= 1 && abs(rect.minY - frame.minY) <= 1
                && abs(rect.width - frame.width) <= 1 && abs(rect.height - frame.height) <= 1
        }
        guard matches.count == 1, let number = matches[0][kCGWindowNumber as String] as? Int else {
            return nil
        }
        return CGWindowID(number)
    }

    private func rect(_ bounds: [String: Int]) -> CGRect {
        CGRect(
            x: bounds["x", default: 0], y: bounds["y", default: 0],
            width: bounds["width", default: 0], height: bounds["height", default: 0]
        )
    }

    private func captureElement(args: Arguments, deadlineMS: Double?) throws -> [String: Any] {
        try args.only(["native_token"])
        try requireTrusted()
        // Checked silently: the helper never raises the Screen Recording prompt.
        guard CGPreflightScreenCaptureAccess() else {
            throw HelperFailure(
                "DRIVER.SCREEN_RECORDING_DENIED",
                "Screen Recording permission has not been granted to MacOSAXHelper.app",
                data: [
                    "reason": "screen_recording_not_granted",
                    "bundle_id": "dev.ai-auto-desktop.macos-ax-helper",
                ]
            )
        }
        let element = try tokens.resolve(try args.string("native_token")!)
        try configureTimeout(element, deadlineMS: deadlineMS)
        let role = stringAttribute(element, kAXRoleAttribute as CFString)
        let subrole = stringAttribute(element, kAXSubroleAttribute as CFString)
        guard role != "AXSecureTextField", subrole != "AXSecureTextField" else {
            throw HelperFailure("DRIVER.PROTECTED_ELEMENT", "protected element is never captured")
        }
        guard let bounds = copyBounds(element), bounds["width", default: 0] > 0,
              bounds["height", default: 0] > 0 else {
            throw HelperFailure("DRIVER.ACTION_UNSUPPORTED", "capture requires positive-area bounds")
        }
        let owner: AXUIElement? = role == "AXWindow"
            ? element : elementAttribute(element, kAXWindowAttribute as CFString)
        guard let window = owner, let windowBounds = copyBounds(window) else {
            throw HelperFailure("DRIVER.CAPTURE_FAILED", "the element's window could not be found")
        }
        if boolAttribute(window, kAXMinimizedAttribute as CFString) == true {
            throw HelperFailure("DRIVER.WINDOW_MINIMIZED", "the window is minimized and shows nothing")
        }
        var pid = pid_t(0)
        try requireSuccess(AXUIElementGetPid(element, &pid), operation: "AXUIElementGetPid")
        let windowRect = rect(windowBounds)
        guard let windowID = cgWindowID(pid: pid, frame: windowRect) else {
            throw HelperFailure(
                "DRIVER.CAPTURE_FAILED", "no single on-screen window of the application has this frame"
            )
        }
        let wanted = rect(bounds)
        var area = wanted.intersection(windowRect)
        if area.width > maximumCapturePoints { area.size.width = maximumCapturePoints }
        if area.height > maximumCapturePoints { area.size.height = maximumCapturePoints }
        guard !area.isNull, area.width > 0, area.height > 0 else {
            throw HelperFailure("DRIVER.ACTION_UNSUPPORTED", "the element lies outside its window")
        }
        try checkDeadline(deadlineMS)
        guard let image = createWindowImage(area, window: windowID) else {
            throw HelperFailure(
                "DRIVER.CAPTURE_FAILED", "CGWindowListCreateImage returned no image",
                data: ["window_id": Int(windowID)]
            )
        }
        guard let png = NSBitmapImageRep(cgImage: image).representation(using: .png, properties: [:])
        else {
            throw HelperFailure("DRIVER.CAPTURE_FAILED", "the capture could not be encoded as PNG")
        }
        return [
            "format": "png",
            "data": png.base64EncodedString(),
            "width": image.width,
            "height": image.height,
            "clipped": area != wanted,
            "window_id": Int(windowID),
        ]
    }

    private func savePasteboard(_ pasteboard: NSPasteboard) throws -> [[(NSPasteboard.PasteboardType, Data)]] {
        var total = 0
        var saved: [[(NSPasteboard.PasteboardType, Data)]] = []
        for item in pasteboard.pasteboardItems ?? [] {
            var entries: [(NSPasteboard.PasteboardType, Data)] = []
            for type in item.types {
                guard let data = item.data(forType: type) else { continue }
                total += data.count
                guard total <= maximumSavedPasteboardBytes else {
                    throw HelperFailure(
                        "DRIVER.CLIPBOARD_TOO_LARGE",
                        "the pasteboard holds more than a paste can preserve; nothing was pasted"
                    )
                }
                entries.append((type, data))
            }
            saved.append(entries)
        }
        return saved
    }

    /// Put the person's pasteboard back unless someone else wrote it since.
    private func restorePasteboard(
        _ pasteboard: NSPasteboard, _ saved: [[(NSPasteboard.PasteboardType, Data)]], offered: Int
    ) -> String {
        guard pasteboard.changeCount == offered else { return "kept_newer" }
        pasteboard.clearContents()
        if saved.isEmpty { return "cleared" }
        let items = saved.map { entries -> NSPasteboardItem in
            let item = NSPasteboardItem()
            for (type, data) in entries { item.setData(data, forType: type) }
            return item
        }
        return pasteboard.writeObjects(items) ? "restored" : "lost"
    }

    private func paste(
        args: Arguments, deadlineMS: Double?, requestID: String,
        progress: RequestProgress
    ) throws -> [String: Any] {
        try args.only(["native_token", "text"])
        try requireTrusted()
        let text = try args.string("text")!
        guard !text.isEmpty, text.count <= maximumPasteCharacters else {
            throw HelperFailure("DRIVER.INVALID_REQUEST", "helper args.text is 1..4096 characters")
        }
        let element = try tokens.resolve(try args.string("native_token")!)
        try configureTimeout(element, deadlineMS: deadlineMS)
        let role = stringAttribute(element, kAXRoleAttribute as CFString)
        let subrole = stringAttribute(element, kAXSubroleAttribute as CFString)
        if role == "AXSecureTextField" || subrole == "AXSecureTextField" {
            throw HelperFailure("DRIVER.PROTECTED_ELEMENT", "paste target is protected")
        }
        guard boolAttribute(element, kAXEnabledAttribute as CFString) != false,
              isSettable(element, kAXFocusedAttribute as CFString) else {
            throw HelperFailure(
                "DRIVER.ACTION_UNSUPPORTED", "paste target is disabled or cannot take focus",
                data: progress.keyboardMetadata(phase: "target_preflight")
            )
        }
        guard !secureEventInputIsEnabled(IsSecureEventInputEnabled()) else {
            throw HelperFailure(
                "DRIVER.PROTECTED_ELEMENT", "macOS Secure Event Input is enabled",
                data: progress.keyboardMetadata(phase: "secure_event_input_preflight")
            )
        }
        var targetPID = pid_t(0)
        try requireSuccess(AXUIElementGetPid(element, &targetPID), operation: "AXUIElementGetPid")
        try requireFrontmost(targetPID, operation: "paste")
        try checkDeadline(deadlineMS)
        let pasteboard = NSPasteboard.general
        let saved = try savePasteboard(pasteboard)
        let provider = PasteProvider(text: text)
        let item = NSPasteboardItem()
        // Clipboard managers that honour nspasteboard.org markers skip this text.
        item.setData(Data(), forType: concealedType)
        item.setData(Data(), forType: transientType)
        guard item.setDataProvider(provider, forTypes: [.string]) else {
            throw HelperFailure("DRIVER.ACTION_FAILED", "the pasteboard refused the text provider")
        }
        pasteboard.clearContents()
        guard pasteboard.writeObjects([item]) else {
            _ = restorePasteboard(pasteboard, saved, offered: pasteboard.changeCount)
            throw HelperFailure("DRIVER.ACTION_FAILED", "the pasteboard refused the text")
        }
        let offered = pasteboard.changeCount
        do {
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
            try requireFrontmost(targetPID, operation: "paste")
            guard !secureEventInputIsEnabled(IsSecureEventInputEnabled()) else {
                throw HelperFailure(
                    "DRIVER.PROTECTED_ELEMENT",
                    "macOS Secure Event Input became enabled before keyboard dispatch",
                    data: progress.keyboardMetadata(phase: "secure_event_input_preflight")
                )
            }
            try checkDeadline(deadlineMS)
            let command = Self.modifierKeys["cmd"]!
            let v = Self.keyCodes["v"]!
            let events = try [
                keyEvent(command.code, down: true, flags: command.flag),
                keyEvent(v, down: true, flags: command.flag),
                keyEvent(v, down: false, flags: command.flag),
                keyEvent(command.code, down: false, flags: CGEventFlags()),
            ]
            progress.markKeyboardDispatchStarted()
            emitKeyboardDispatchProgress(
                id: requestID, keyboardDispatchStarted: true, focusChanged: progress.focusChanged
            )
            for event in events { event.postToPid(targetPID) }
        } catch {
            _ = restorePasteboard(pasteboard, saved, offered: offered)
            throw error
        }
        let consumed = waitUntil(deadlineMS) { provider.provided }
        // Let the target finish reading before the pasteboard changes again.
        RunLoop.current.run(until: Date().addingTimeInterval(0.1))
        let restored = restorePasteboard(pasteboard, saved, offered: offered)
        return [
            "native_operation": "NSPasteboard+CGEventCmdV",
            "submitted": true,
            "keyboard_dispatch_started": progress.keyboardDispatchStarted,
            "focus_changed": progress.focusChanged,
            "phase": "submitted",
            "consumed": consumed,
            "restored": restored,
        ]
    }

    private func clipboardRead(args: Arguments) throws -> [String: Any] {
        try args.only(["max_characters"])
        let maximum = try args.integer("max_characters", minimum: 1, maximum: maximumClipboardCharacters)
        let pasteboard = NSPasteboard.general
        if (pasteboard.types ?? []).contains(concealedType) {
            throw HelperFailure(
                "DRIVER.CLIPBOARD_PROTECTED",
                "the pasteboard holds content marked concealed (a password manager)"
            )
        }
        guard let text = pasteboard.string(forType: .string) else {
            return ["has_text": false, "text": NSNull(), "characters": 0, "truncated": false]
        }
        let count = text.count
        return [
            "has_text": true, "text": String(text.prefix(maximum)),
            "characters": count, "truncated": count > maximum,
        ]
    }

    private func globalKey(
        args: Arguments, deadlineMS: Double?, requestID: String,
        progress: RequestProgress
    ) throws -> [String: Any] {
        try args.only(["keys"])
        try requireTrusted()
        guard let raw = args.values["keys"] as? [String], (1...6).contains(raw.count) else {
            throw HelperFailure("DRIVER.INVALID_REQUEST", "helper args.keys must hold 1..6 names")
        }
        let names = raw.map { $0.lowercased() }
        var modifiers: [(code: CGKeyCode, flag: CGEventFlags)] = []
        for name in names.dropLast() {
            guard let modifier = Self.modifierKeys[name] else {
                throw HelperFailure(
                    "DRIVER.INVALID_REQUEST", "only modifiers may precede the main key",
                    data: ["key": name]
                )
            }
            modifiers.append(modifier)
        }
        guard let main = Self.keyCodes[names[names.count - 1]] else {
            throw HelperFailure(
                "DRIVER.ACTION_UNSUPPORTED", "the main key has no macOS key code here",
                data: ["key": names[names.count - 1]]
            )
        }
        guard !secureEventInputIsEnabled(IsSecureEventInputEnabled()) else {
            throw HelperFailure(
                "DRIVER.PROTECTED_ELEMENT", "macOS Secure Event Input is enabled",
                data: progress.keyboardMetadata(phase: "secure_event_input_preflight")
            )
        }
        var flags = CGEventFlags()
        var events: [CGEvent] = []
        for modifier in modifiers {
            flags.insert(modifier.flag)
            events.append(try keyEvent(modifier.code, down: true, flags: flags))
        }
        events.append(try keyEvent(main, down: true, flags: flags))
        events.append(try keyEvent(main, down: false, flags: flags))
        for modifier in modifiers.reversed() {
            flags.remove(modifier.flag)
            events.append(try keyEvent(modifier.code, down: false, flags: flags))
        }
        try checkDeadline(deadlineMS)
        let frontmost = NSWorkspace.shared.frontmostApplication?.processIdentifier ?? 0
        progress.markKeyboardDispatchStarted()
        emitKeyboardDispatchProgress(id: requestID, keyboardDispatchStarted: true, focusChanged: false)
        for event in events { event.post(tap: .cghidEventTap) }
        return [
            "native_operation": "CGEventGlobalKeyChord",
            "submitted": true,
            "keyboard_dispatch_started": progress.keyboardDispatchStarted,
            "focus_changed": progress.focusChanged,
            "phase": "submitted",
            "frontmost_process_id": Int(frontmost),
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
        // A pipe request is usually much smaller than 64 KiB and the client
        // keeps stdin open for the next RPC. readData(ofLength:) may wait to
        // fill that length, deadlocking both sides until the RPC deadline.
        let chunk = input.availableData
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
            guard keyboardOperations.contains(operation) || pointerOperations.contains(operation)
            else { throw failure }
            var data = failure.data
            let defaults = keyboardOperations.contains(operation)
                ? progress.keyboardMetadata(phase: data["phase"] as? String ?? "pre_dispatch")
                : progress.pointerMetadata(phase: data["phase"] as? String ?? "pre_dispatch")
            for (key, value) in defaults where data[key] == nil { data[key] = value }
            throw HelperFailure(
                failure.code, failure.message, retryable: failure.retryable, data: data
            )
        } catch {
            guard keyboardOperations.contains(operation) || pointerOperations.contains(operation)
            else { throw error }
            let phase = progress.keyboardDispatchStarted
                ? "keyboard_dispatch"
                : progress.pointerDispatchStarted
                ? "pointer_dispatch"
                : "pre_dispatch"
            var data: [String: Any] = ["exception_type": String(describing: type(of: error))]
            if keyboardOperations.contains(operation) {
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
