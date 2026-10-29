import AppKit
import RunnerManagerKit
import SwiftUI

/// Renders the main screens to PNG files (`--snapshot <directory>`), for the README and reviews.
/// Every screen is rendered from the demo service, without opening a visible window.
@MainActor
enum Snapshots {
    static let size = NSSize(width: 1040, height: 680)

    static func render(to directory: URL) {
        NSApplication.shared.setActivationPolicy(.prohibited)
        try? FileManager.default.createDirectory(at: directory, withIntermediateDirectories: true)

        let populated = loadedModel(.populated)
        let runners = populated.runners.value ?? []
        let byName = Dictionary(uniqueKeysWithValues: runners.map { ($0.name, $0.id) })

        let scenes: [(String, AnyView, NSSize)] = [
            ("01-overview", AnyView(ContentView(model: populated)), size),
            ("02-runner-running", AnyView(ContentView(model: populated, selection: byName["studio-build"].map(SidebarItem.runner))), size),
            ("03-runner-failed", AnyView(ContentView(model: populated, selection: byName["studio-gpu"].map(SidebarItem.runner))), size),
            ("04-add-runner", AnyView(AddRunnerSheet(model: populated, form: sampleForm(), onAdded: { _ in })), NSSize(width: 460, height: 340)),
            ("05-not-associated", AnyView(ContentView(model: loadedModel(.notAssociated))), size),
            ("06-no-runner", AnyView(ContentView(model: loadedModel(.empty))), size),
            ("07-service-unavailable", AnyView(ContentView(model: loadedModel(.unavailable))), size),
            ("08-loading", AnyView(ContentView(model: RunnerManagerModel(service: DemoRunnerService(latency: .seconds(3600))))), size),
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
