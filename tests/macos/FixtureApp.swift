import AppKit
import Darwin

private let fixtureWindowTitle = "AI Auto Desktop macOS AX Fixture"
private let initialValue = "Draft"
private let initialStatus = "Status: idle"

private func configureLifecycle() -> Bool {
    let arguments = CommandLine.arguments
    guard let marker = arguments.firstIndex(of: "--parent-pid"),
          marker + 1 < arguments.count,
          let parentPID = Int32(arguments[marker + 1]),
          parentPID > 1,
          parentPID == getppid(),
          setpgid(0, getpgrp()) == 0 || getpgrp() == getpgid(parentPID)
    else { return false }
    return true
}

final class FixtureAppDelegate: NSObject, NSApplicationDelegate {
    private var window: NSWindow!
    private var input: NSTextField!
    private var status: NSTextField!

    func applicationDidFinishLaunching(_ notification: Notification) {
        window = NSWindow(
            contentRect: NSRect(x: 0, y: 0, width: 520, height: 250),
            styleMask: [.titled, .closable, .miniaturizable],
            backing: .buffered,
            defer: false
        )
        window.title = fixtureWindowTitle
        window.center()

        let label = NSTextField(labelWithString: "Fixture input")
        label.frame = NSRect(x: 24, y: 184, width: 180, height: 22)

        input = NSTextField(string: initialValue)
        input.frame = NSRect(x: 24, y: 146, width: 310, height: 28)
        input.setAccessibilityIdentifier("fixture-input")
        input.setAccessibilityLabel("Fixture Input")

        let button = NSButton(
            title: "Apply Fixture Value",
            target: self,
            action: #selector(applyFixtureValue)
        )
        button.frame = NSRect(x: 24, y: 96, width: 190, height: 32)
        button.bezelStyle = .rounded
        button.setAccessibilityIdentifier("fixture-apply")

        let duplicateButton = NSButton(
            title: "Apply Fixture Value",
            target: nil,
            action: nil
        )
        duplicateButton.frame = NSRect(x: 230, y: 96, width: 190, height: 32)
        duplicateButton.bezelStyle = .rounded
        duplicateButton.setAccessibilityIdentifier("fixture-duplicate")

        status = NSTextField(labelWithString: initialStatus)
        status.frame = NSRect(x: 24, y: 50, width: 450, height: 24)
        status.setAccessibilityIdentifier("fixture-status")

        guard let contentView = window.contentView else {
            NSApp.terminate(nil)
            return
        }
        contentView.addSubview(label)
        contentView.addSubview(input)
        contentView.addSubview(button)
        contentView.addSubview(duplicateButton)
        contentView.addSubview(status)

        window.makeKeyAndOrderFront(nil)
        NSApp.activate(ignoringOtherApps: true)
    }

    @objc private func applyFixtureValue() {
        status.stringValue = "Status: pressed: \(input.stringValue)"
    }

    func applicationShouldTerminateAfterLastWindowClosed(_ sender: NSApplication) -> Bool {
        true
    }
}

let app = NSApplication.shared
guard configureLifecycle() else { exit(1) }
let delegate = FixtureAppDelegate()
app.delegate = delegate
app.setActivationPolicy(.regular)
app.run()

