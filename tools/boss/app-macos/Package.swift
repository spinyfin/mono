// swift-tools-version: 6.2
import PackageDescription

let package = Package(
    name: "BossMacApp",
    platforms: [.macOS(.v15)],
    products: [
        .executable(name: "BossMacApp", targets: ["BossMacApp"]),
    ],
    dependencies: [
        .package(url: "https://github.com/gonzalezreal/textual", from: "0.1.0"),
    ],
    targets: [
        .executableTarget(
            name: "BossMacApp",
            dependencies: [
                .product(name: "Textual", package: "textual"),
            ],
            path: "Sources"
        ),
    ]
)
