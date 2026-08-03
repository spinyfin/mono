enum BossEngineBinary {
    static let executableName = "engine"
    static let bazelRunCommand = "bazel run //tools/boss/engine/core:engine"
    static let bazelOutputPathFragment = "/tools/boss/engine/core/engine"
    static let bundlePathFragment = "Contents/Resources/bin/engine"
}
