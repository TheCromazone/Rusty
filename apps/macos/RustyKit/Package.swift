// swift-tools-version: 6.0
import PackageDescription
let package = Package(
    name: "RustyKit", platforms: [.macOS(.v14)],
    products: [.library(name: "RustyProtocol", targets: ["RustyProtocol"])],
    targets: [.target(name: "RustyProtocol"),
              .testTarget(name: "RustyProtocolTests", dependencies: ["RustyProtocol"], resources: [.copy("Fixtures")])]
)
