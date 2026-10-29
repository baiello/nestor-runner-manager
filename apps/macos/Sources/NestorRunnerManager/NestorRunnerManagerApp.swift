import AppKit
import RunnerManagerKit
import SwiftUI

@main
enum Entry {
    static func main() {
        let options = LaunchOptions(arguments: CommandLine.arguments)
        if let directory = options.snapshotDirectory {
            MainActor.assumeIsolated {
                if let socket = options.socketPath {
                    Snapshots.renderService(at: socket, prefix: options.snapshotPrefix, to: directory)
                } else {
                    Snapshots.render(to: directory)
                }
            }
            return
        }
        NestorRunnerManagerApp.main()
    }
}

/// Command-line options: the demo's other states, or the local service.
struct LaunchOptions {
    var scenario: DemoRunnerService.Scenario = .populated
    var snapshotDirectory: URL?
    var snapshotPrefix = "service"
    /// Set by `--service` (default socket) or `--socket <path>`: use the local service of `core/`
    /// instead of the in-app demo.
    var socketPath: String?

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
            case "--snapshot-prefix":
                if let value = iterator.next() { snapshotPrefix = value }
            case "--service":
                socketPath = socketPath ?? LiveRunnerService.defaultSocketPath
            case "--socket":
                if let value = iterator.next() { socketPath = value }
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
        // The in-app demo by default; `--service` or `--socket` reach the local service, whose own
        // report decides whether the demo banner shows.
        let service: any RunnerService = if let socket = options.socketPath {
            LiveRunnerService(socketPath: socket)
        } else {
            DemoRunnerService(scenario: options.scenario)
        }
        _model = State(initialValue: RunnerManagerModel(service: service))
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
