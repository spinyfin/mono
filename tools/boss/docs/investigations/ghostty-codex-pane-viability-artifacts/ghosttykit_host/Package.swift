// swift-tools-version: 6.0
// Throwaway GhosttyKit embed harness for the ghostty+codex pane viability spike.
// Uses the same embedding surface Boss uses (GhosttyKit / libghostty), not
// standalone Ghostty.app + outsider shell_pid observation.
import PackageDescription

let package = Package(
    name: "GhosttyKitSpike",
    platforms: [.macOS(.v15)],
    products: [
        .executable(name: "ghosttykit_spike", targets: ["ghosttykit_spike"]),
    ],
    targets: [
        .binaryTarget(
            name: "GhosttyKit",
            path: ".local-GhosttyKit.xcframework"
        ),
        .executableTarget(
            name: "ghosttykit_spike",
            dependencies: ["GhosttyKit"],
            path: "Sources",
            swiftSettings: [
                // Throwaway harness: keep AppKit + C-callback interop simple.
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
