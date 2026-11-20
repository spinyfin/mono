//! Real GC history: a small compatibility fixture or the published scale fixture.
use super::Fixture;
use crate::app::test_fs::copy_tree;
use std::fs;
use std::path::Path;

fn cut_path() -> String {
    std::env::var("CUBE_GC_STORE_CUT").expect("Bazel must select the GC fixture")
}

pub(super) fn retained_operations() -> u64 {
    if cut_path().is_empty() { 16 } else { 16_800 }
}

pub(super) fn unreachable_views() -> u64 {
    // Collector/lease startup is synchronized, so compatibility does not need
    // the oldest-jj variant's neglected-store scale to ensure overlap.
    if cut_path().is_empty() { 256 } else { 137_000 }
}

pub(super) fn install(f: &Fixture) -> String {
    let cut = cut_path();
    if cut.is_empty() {
        // Rewrite selection depends on age, not log size. The runner ages
        // this cut past DEFAULT_KEEP and keeps all subsequent operations young.
        let cut = f.jj(
            &f.root,
            &[
                "--ignore-working-copy",
                "op",
                "log",
                "--no-graph",
                "--limit",
                "1",
                "-T",
                "id",
            ],
        );
        for i in 0..retained_operations() {
            let rev = if i % 2 == 0 { "main" } else { "feat" };
            f.jj(
                &f.ws,
                &[
                    "--ignore-working-copy",
                    "bookmark",
                    "set",
                    "gc-compat",
                    "--allow-backwards",
                    "-r",
                    rev,
                ],
            );
        }
        return cut.trim().to_owned();
    }
    let source = Path::new(&cut).parent().expect("fixture directory containing cut");
    // Keep the small-store lease/release baseline and its database. Replace
    // only the scratch repositories at the registered paths with the large
    // history from the published fixture.
    for relative in ["root", "upstream", "pool/mono-agent-001"] {
        let target = f._dir.path().join(relative);
        fs::remove_dir_all(&target).unwrap();
        copy_tree(&source.join(relative), &target).unwrap();
    }
    // These are the only links to the generator's directory. All objects,
    // operation history, views and working-copy state remain real jj data.
    fs::write(f.ws.join(".jj/repo"), f.root.join(".jj/repo").to_str().unwrap()).unwrap();
    let upstream = f._dir.path().join("upstream");
    f.jj(
        &f.root,
        &["git", "remote", "set-url", "origin", upstream.to_str().unwrap()],
    );
    f.jj(
        &upstream,
        &["git", "remote", "set-url", "origin", f.root.to_str().unwrap()],
    );
    fs::read_to_string(source.join("cut")).unwrap().trim().to_owned()
}
