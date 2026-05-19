// swift-tools-version: 6.0
import PackageDescription

let package = Package(
    name: "textual-perf",
    platforms: [.macOS(.v15)],
    products: [
        .executable(name: "textualperf", targets: ["TextualPerf"]),
    ],
    dependencies: [
        .package(url: "https://github.com/gonzalezreal/textual", from: "0.3.1"),
    ],
    targets: [
        .executableTarget(
            name: "TextualPerf",
            dependencies: [
                .product(name: "Textual", package: "textual"),
            ],
            path: "Sources/TextualPerf",
            exclude: ["Resources/Info.plist"],
            resources: [
                .copy("Resources/sample-46kb.md"),
                .copy("Resources/sample-nocode.md"),
                .copy("Resources/sample-1kb.md"),
            ]
        ),
    ]
)
