fn main() {
    // Release pipeline sets CHECKLEFT_VERSION before building.
    // Emit CHECKLEFT_BUILD_VERSION so that option_env!("CHECKLEFT_BUILD_VERSION")
    // in main.rs resolves to the tag version rather than the Cargo.toml placeholder.
    // Dev Cargo builds that don't set CHECKLEFT_VERSION fall through to CARGO_PKG_VERSION.
    if let Ok(v) = std::env::var("CHECKLEFT_VERSION") {
        println!("cargo:rustc-env=CHECKLEFT_BUILD_VERSION={v}");
    }
    println!("cargo:rerun-if-env-changed=CHECKLEFT_VERSION");
}
