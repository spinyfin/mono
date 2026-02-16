// swift-tools-version: 6.2
import PackageDescription

let package = Package(
    name: "BossMacApp",
    platforms: [.macOS(.v14)],
    products: [
        .executable(name: "BossMacApp", targets: ["BossMacApp"]),
    ],
    targets: [
        .executableTarget(
            name: "BossMacApp",
            path: "Sources"
        ),
    ]
)
