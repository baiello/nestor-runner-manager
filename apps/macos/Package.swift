// swift-tools-version: 6.0
import PackageDescription

let package = Package(
    name: "NestorRunnerManager",
    platforms: [.macOS(.v14)],
    products: [
        .executable(name: "NestorRunnerManager", targets: ["NestorRunnerManager"]),
        .library(name: "RunnerManagerKit", targets: ["RunnerManagerKit"]),
    ],
    targets: [
        // Models, the `RunnerService` contract the views depend on, and the demo service.
        // No SwiftUI here: a future implementation backed by the shared Rust core plugs in
        // behind `RunnerService` without touching the interface.
        .target(name: "RunnerManagerKit"),
        // The SwiftUI application. It only talks to `RunnerService`.
        .executableTarget(
            name: "NestorRunnerManager",
            dependencies: ["RunnerManagerKit"]
        ),
        .testTarget(
            name: "RunnerManagerKitTests",
            dependencies: ["RunnerManagerKit"]
        ),
    ]
)
