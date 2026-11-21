import AppKit
import Foundation

// The fixture reports AppKit's actual completion and URL. It stays alive
// after dismissal so the driver's postcondition can inspect the host window.
let mode = CommandLine.arguments[1]
let directory = URL(fileURLWithPath: CommandLine.arguments[2], isDirectory: true)
let app = NSApplication.shared
app.setActivationPolicy(.regular)
let window = NSWindow(contentRect: NSRect(x: 120, y: 140, width: 760, height: 540),
                      styleMask: [.titled, .closable, .resizable], backing: .buffered, defer: false)
window.title = "Native picker fixture"
window.center()
window.makeKeyAndOrderFront(nil)
app.activate(ignoringOtherApps: true)

func report(_ name: String, _ value: Any) {
    do {
        let data = try JSONSerialization.data(withJSONObject: value)
        try data.write(to: directory.appendingPathComponent(name), options: .atomic)
    } catch {
        fatalError("fixture could not report \(name): \(error)")
    }
}

let panel = mode == "save" ? NSSavePanel() : NSOpenPanel()
panel.directoryURL = directory
if mode == "directory", let open = panel as? NSOpenPanel {
    open.canChooseFiles = false
    open.canChooseDirectories = true
    open.message = "Select a folder this site can view"
    open.prompt = "Choose"
}
if mode == "save" {
    panel.nameFieldStringValue = "suggestion.txt"
    panel.isExtensionHidden = false
}
let alert = NSAlert()
alert.messageText = "Replace the existing document?"
alert.addButton(withTitle: "Replace")
alert.addButton(withTitle: "Cancel")

DispatchQueue.main.async {
    panel.beginSheetModal(for: window) { result in
        if mode == "replace" {
            alert.beginSheetModal(for: window) { _ in report("prompt-result", ["pressed": true]) }
            report("replaced", ["attached": window.attachedSheet === alert.window])
            return
        }
        guard result == .OK, let url = panel.url else {
            report("result", ["cancelled": result == .cancel])
            return
        }
        do {
            if let open = panel as? NSOpenPanel {
                if mode == "directory" {
                    report("result", ["selected": open.urls.map(\.path)])
                    return
                }
                report("result", ["selected": open.urls.map(\.path), "contents": try String(contentsOf: url, encoding: .utf8)])
                return
            }
            try Data("native fixture wrote this file\n".utf8).write(to: url, options: .withoutOverwriting)
            report("result", ["selected": url.path])
        } catch {
            report("result", ["error": String(describing: error)])
        }
    }
    let frame = window.frame
    let bounds = [frame.minX, NSScreen.screens[0].frame.maxY - frame.maxY, frame.width, frame.height]
    report("ready", bounds.map { Int($0.rounded()) })
}
if mode == "replace" {
    Timer.scheduledTimer(withTimeInterval: 0.05, repeats: true) { timer in
        if FileManager.default.fileExists(atPath: directory.appendingPathComponent("replace-now").path) {
            timer.invalidate()
            panel.cancel(nil)
        }
    }
}
app.run()
