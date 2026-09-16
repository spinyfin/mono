use super::*;

#[test]
fn every_generated_launcher_pins_an_absolute_quoted_exec_target() {
    let tmp = tempfile::tempdir().unwrap();
    for target in [Some(Path::new("/opt/Boss's tools/$(literal)/cli")), None] {
        for launcher in [
            write_boss_launcher(tmp.path(), target).unwrap(),
            write_cube_launcher(tmp.path(), target).unwrap(),
        ] {
            let script = std::fs::read_to_string(launcher).unwrap();
            let execs: Vec<_> = script
                .lines()
                .filter_map(|line| line.trim().strip_prefix("exec "))
                .collect();
            assert_eq!(execs.len(), usize::from(target.is_some()), "{script}");
            for exec in execs {
                assert!(
                    exec.starts_with("'/"),
                    "exec target must be absolute and quoted: {exec}"
                );
                assert_eq!(exec, format!("{} \"$@\"", sh_quote(&target.unwrap().to_string_lossy())));
            }
            assert!(!script.contains("exec cube "), "{script}");
            assert!(!script.contains("exec boss "), "{script}");
            assert!(
                !script.contains("boss-pr-body"),
                "launchers must not compose PR bodies: {script}"
            );
        }
    }
}

#[test]
fn normal_cube_write_replaces_legacy_interception_script() {
    let tmp = tempfile::tempdir().unwrap();
    let legacy = "#!/bin/sh\n# Legacy body interception\nexec cube \"$@\"\n";
    for target in [Some(Path::new("/opt/bin/cube")), None] {
        std::fs::write(tmp.path().join("cube"), legacy).unwrap();
        let launcher = write_cube_launcher(tmp.path(), target).unwrap();
        let script = std::fs::read_to_string(launcher).unwrap();
        assert_eq!(script, launcher_script(target));
        assert!(!script.contains("Legacy body interception"));
        assert!(!script.contains("exec cube "));
    }
}
