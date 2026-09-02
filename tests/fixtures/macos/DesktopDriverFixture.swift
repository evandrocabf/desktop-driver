import AppKit

final class FixtureDelegate: NSObject, NSApplicationDelegate {
    private var window: NSWindow!
    private var input: NSTextField!
    private var result: NSTextField!

    func applicationDidFinishLaunching(_ notification: Notification) {
        window = NSWindow(
            contentRect: NSRect(x: 0, y: 0, width: 520, height: 240),
            styleMask: [.titled, .closable, .miniaturizable],
            backing: .buffered,
            defer: false
        )
        window.title = "desktop-driver macOS fixture"
        window.center()

        input = NSTextField(frame: NSRect(x: 40, y: 145, width: 440, height: 28))
        input.setAccessibilityLabel("Input")
        input.stringValue = "ready"

        let button = NSButton(frame: NSRect(x: 40, y: 90, width: 120, height: 32))
        button.title = "Commit"
        button.bezelStyle = .rounded
        button.target = self
        button.action = #selector(commit)

        result = NSTextField(labelWithString: "Result: waiting")
        result.frame = NSRect(x: 40, y: 45, width: 440, height: 28)
        result.setAccessibilityLabel("Result")

        window.contentView?.addSubview(input)
        window.contentView?.addSubview(button)
        window.contentView?.addSubview(result)
        window.makeKeyAndOrderFront(nil)
        NSApp.activate(ignoringOtherApps: true)
    }

    @objc private func commit() {
        result.stringValue = "Result: \(input.stringValue)"
    }
}

let application = NSApplication.shared
let delegate = FixtureDelegate()
application.delegate = delegate
application.setActivationPolicy(.regular)
application.run()
