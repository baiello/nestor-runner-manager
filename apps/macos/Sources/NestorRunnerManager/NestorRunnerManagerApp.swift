import AppKit
import RunnerManagerKit
import SwiftUI

@main
enum Entry {
    static func main() {
        let options = LaunchOptions(arguments: CommandLine.arguments)
        if let directory = options.snapshotDirectory {
            MainActor.assumeIsolated {
                Snapshots.render(to: directory)
            }
            return
        }
        NestorRunnerManagerApp.main()
    }
}

/// Command-line options, mostly to show the demo's other states.
struct LaunchOptions {
    var scenario: DemoRunnerService.Scenario = .populated
    var snapshotDirectory: URL?

    init(arguments: [String]) {
        var iterator = arguments.dropFirst().makeIterator()
        while let argument = iterator.next() {
            switch argument {
            case "--demo-scenario":
                if let value = iterator.next(), let scenario = DemoRunnerService.Scenario(rawValue: value) {
                    self.scenario = scenario
                }
            case "--snapshot":
                if let value = iterator.next() {
                    snapshotDirectory = URL(fileURLWithPath: value, isDirectory: true)
                }
            default:
                break
            }
        }
    }
}

struct NestorRunnerManagerApp: App {
    @NSApplicationDelegateAdaptor(AppDelegate.self) private var appDelegate
    @State private var model: RunnerManagerModel

    init() {
        let options = LaunchOptions(arguments: CommandLine.arguments)
        // The only service available today. Replace with the live service once the Rust core exists.
        _model = State(initialValue: RunnerManagerModel(service: DemoRunnerService(scenario: options.scenario)))
    }

    var body: some Scene {
        WindowGroup("Nestor Runner Manager") {
            ContentView(model: model)
                .frame(minWidth: 860, minHeight: 540)
        }
        .windowResizability(.contentMinSize)
    }
}

/// An executable built by SwiftPM has no bundle; make it a regular app with a Dock icon and menus.
final class AppDelegate: NSObject, NSApplicationDelegate {
    func applicationWillFinishLaunching(_ notification: Notification) {
        NSApp.setActivationPolicy(.regular)
    }

    func applicationDidFinishLaunching(_ notification: Notification) {
        NSApp.activate()
    }

    func applicationShouldTerminateAfterLastWindowClosed(_ sender: NSApplication) -> Bool {
        true
    }
}
