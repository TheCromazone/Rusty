// swift-tools-version: 6.0
import PackageDescription
let package = Package(name: "DiffPrototype", platforms: [.macOS(.v14)], products: [
    .library(name: "DiffPrototypeCore", targets: ["DiffPrototypeCore"]),
    .executable(name: "diff-prototype", targets: ["DiffPrototype"])
], targets: [
    .target(name: "DiffPrototypeCore"),
    .executableTarget(name: "DiffPrototype", dependencies: ["DiffPrototypeCore"]),
    .testTarget(name: "DiffPrototypeCoreTests", dependencies: ["DiffPrototypeCore"]),
    .testTarget(name: "DiffPrototypeLifecycleTests", dependencies: ["DiffPrototype"])
])
