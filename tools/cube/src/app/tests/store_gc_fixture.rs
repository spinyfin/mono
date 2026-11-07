//! Install a private copy of the Bazel-built large operation history.
use super::Fixture;
use std::fs;
use std::path::Path;

fn copy_tree(source: &Path, destination: &Path) -> std::io::Result<()> {
    fs::create_dir_all(destination)?;
    for entry in fs::read_dir(source)? {
        let entry = entry?;
        let target = destination.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_tree(&entry.path(), &target)?;
        } else {
            assert!(
                entry.file_type()?.is_file(),
                "fixture must contain only directories and files"
            );
            // Bazel artifacts are read-only; fresh files must be writable by jj.
            fs::write(target, fs::read(entry.path())?)?;
        }
    }
    Ok(())
}

pub(super) fn install(f: &Fixture) -> String {
    let source = std::env::var("CUBE_GC_STORE_FIXTURE").expect("Bazel must declare the GC fixture");
    let source = Path::new(&source);
    // Keep the small-store lease/release baseline and its database. Replace
    // only the scratch repositories at the registered paths with the large
    // history, just as the old per-run bookmark loop populated those paths.
    for relative in ["root", "upstream", "pool/mono-agent-001"] {
        let target = f._dir.path().join(relative);
        fs::remove_dir_all(&target).unwrap();
        copy_tree(&source.join(relative), &target).unwrap();
    }
    // These are the only links to the build action's directory. All objects,
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
