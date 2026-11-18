//! Install a private copy of the published large operation history.
use super::Fixture;
use crate::app::test_fs::copy_tree;
use std::fs;
use std::path::Path;

pub(super) fn install(f: &Fixture) -> String {
    let cut = std::env::var("CUBE_GC_STORE_CUT").expect("Bazel must declare the GC store cut file");
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
