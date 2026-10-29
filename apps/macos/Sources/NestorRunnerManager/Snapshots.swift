import AppKit
import RunnerManagerKit
import SwiftUI

/// Renders the main screens to PNG files (`--snapshot <directory>`), for the README and reviews.
/// Every screen is rendered from the demo service, without opening a visible window.
@MainActor
enum Snapshots {
    static let size = NSSize(width: 1040, height: 680)
    static let tall = NSSize(width: 1040, height: 1020)

    static func render(to directory: URL) {
        NSApplication.shared.setActivationPolicy(.prohibited)
        try? FileManager.default.createDirectory(at: directory, withIntermediateDirectories: true)

        let populated = loadedModel(.populated)
        let runners = populated.runners.value ?? []
        let byName = Dictionary(uniqueKeysWithValues: runners.map { ($0.name, $0.id) })
        let paused = loadedModel(.populated)
        perform { await paused.setMode(.paused) }

        let scenes: [(String, AnyView, NSSize)] = [
            ("01-machine", AnyView(ContentView(model: populated)), tall),
            ("02-machine-paused", AnyView(ContentView(model: paused)), tall),
            ("03-diagnostics", AnyView(ContentView(model: populated, selection: .diagnostics)), size),
            ("04-runner-busy", AnyView(ContentView(model: populated, selection: byName["runner-1"].map(SidebarItem.runner))), size),
            ("05-runner-failed", AnyView(ContentView(model: populated, selection: byName["runner-3"].map(SidebarItem.runner))), size),
            ("06-add-runner", AnyView(AddRunnerSheet(model: populated, form: sampleForm(), onAdded: { _ in })), NSSize(width: 460, height: 340)),
            ("07-not-associated", AnyView(ContentView(model: loadedModel(.notAssociated))), tall),
            ("08-service-unavailable", AnyView(ContentView(model: loadedModel(.unavailable))), size),
            ("09-loading", AnyView(ContentView(model: RunnerManagerModel(service: DemoRunnerService(latency: .seconds(3600))))), size),
        ]

        for (name, view, size) in scenes {
            let url = directory.appendingPathComponent("\(name).png")
            if write(view, size: size, to: url) {
                print("wrote \(url.path)")
            } else {
                print("failed \(url.path)")
            }
        }
    }

    /// Renders the machine view and the diagnostics from a running local service
    /// (`--snapshot <dir> --socket <path> [--snapshot-prefix <name>]`).
    static func renderService(at socket: String, prefix: String, to directory: URL) {
        NSApplication.shared.setActivationPolicy(.prohibited)
        try? FileManager.default.createDirectory(at: directory, withIntermediateDirectories: true)
        let model = RunnerManagerModel(service: LiveRunnerService(socketPath: socket))
        perform { await model.load() }
        let scenes: [(String, AnyView, NSSize)] = [
            ("\(prefix)-machine", AnyView(ContentView(model: model)), tall),
            ("\(prefix)-diagnostics", AnyView(ContentView(model: model, selection: .diagnostics)), size),
        ]
        for (name, view, size) in scenes {
            let url = directory.appendingPathComponent("\(name).png")
            print(write(view, size: size, to: url) ? "wrote \(url.path)" : "failed \(url.path)")
        }
    }

    private static func perform(_ work: @escaping @MainActor () async -> Void) {
        var done = false
        Task {
            await work()
            done = true
        }
        spin(until: { done })
    }

    private static func sampleForm() -> AddRunnerForm {
        let form = AddRunnerForm()
        form.name = "studio-ci"
        return form
    }

    private static func loadedModel(_ scenario: DemoRunnerService.Scenario) -> RunnerManagerModel {
        let model = RunnerManagerModel(service: DemoRunnerService(scenario: scenario, latency: .zero))
        var done = false
        Task {
            await model.load()
            done = true
        }
        spin(until: { done })
        return model
    }

    private static func write(_ view: AnyView, size: NSSize, to url: URL) -> Bool {
        let host = NSHostingView(rootView: view.frame(width: size.width, height: size.height))
        host.frame = NSRect(origin: .zero, size: size)
        let window = NSWindow(contentRect: host.frame, styleMask: [.titled], backing: .buffered, defer: false)
        window.contentView = host
        window.appearance = NSAppearance(named: .aqua)
        host.layoutSubtreeIfNeeded()
        // Let SwiftUI run its tasks and lay the lists out.
        spin(for: 1.0)
        host.layoutSubtreeIfNeeded()
        guard let bitmap = host.bitmapImageRepForCachingDisplay(in: host.bounds) else { return false }
        host.cacheDisplay(in: host.bounds, to: bitmap)
        guard let data = bitmap.representation(using: .png, properties: [:]) else { return false }
        return (try? data.write(to: url)) != nil
    }

    private static func spin(until condition: () -> Bool, timeout: TimeInterval = 5) {
        let deadline = Date().addingTimeInterval(timeout)
        while !condition() && Date() < deadline {
            RunLoop.main.run(until: Date().addingTimeInterval(0.02))
        }
    }

    private static func spin(for seconds: TimeInterval) {
        RunLoop.main.run(until: Date().addingTimeInterval(seconds))
    }
}
