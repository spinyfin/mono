// swift-tools-version: 6.0
// Throwaway GhosttyKit embed harness for Grok TUI liveness-marker capture.
// Investigation artifact only — not production Boss code.
// Uses the same embedding surface Boss uses (GhosttyKit / libghostty).
import PackageDescription

let package = Package(
    name: "GhosttyKitLiveness",
    platforms: [.macOS(.v15)],
    products: [
        .executable(name: "ghosttykit_liveness", targets: ["ghosttykit_liveness"]),
    ],
    targets: [
        .binaryTarget(
            name: "GhosttyKit",
            path: ".local-GhosttyKit.xcframework"
        ),
        .executableTarget(
            name: "ghosttykit_liveness",
            dependencies: ["GhosttyKit"],
            path: "Sources",
            swiftSettings: [
                .swiftLanguageMode(.v5),
            ],
            linkerSettings: [
                .linkedFramework("AppKit"),
                .linkedFramework("Carbon"),
                .linkedFramework("GameController"),
                .linkedFramework("Metal"),
                .linkedFramework("MetalKit"),
                .linkedFramework("QuartzCore"),
                .linkedFramework("CoreText"),
                .linkedFramework("CoreGraphics"),
                .linkedFramework("IOKit"),
                .linkedLibrary("c++"),
            ]
        ),
    ]
)
