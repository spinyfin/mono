fn main() {
    // In Cargo builds, BOSS_BUILD_INFO_RS is not set by Bazel.
    // Point to the checked-in default file so include!(env!(...)) compiles.
    // Bazel overrides this via rustc_env before running rustc, so build.rs
    // is never executed during Bazel builds.
    if std::env::var("BOSS_BUILD_INFO_RS").is_err() {
        let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR not set");
        // Shared default constants live in the boss-build-info crate so
        // all three Boss binaries fall back to one file (../build-info).
        println!(
            "cargo:rustc-env=BOSS_BUILD_INFO_RS={}/../build-info/src/build_info_default.rs",
            manifest_dir
        );
    }
    println!("cargo:rerun-if-env-changed=BOSS_BUILD_INFO_RS");
}
