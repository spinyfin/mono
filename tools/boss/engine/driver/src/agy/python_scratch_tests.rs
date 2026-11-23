//! A Python body that names this run's scratch must not be able to rearrange
//! scratch and read through the result in the same command, and a path the
//! body builds from the home directory at run time gets no allowance.

use super::scratch_and_quoting_tests::{
    assert_allowed, assert_denied, command_in, prepare_link_via_command, relative_from, run_approved,
};
use super::*;

const CONVERSATION: &str = "c1";

struct Prepared {
    fx: Fixture,
    scratch: String,
    /// `[data, hooks]` relative links planted by approved `ln -s` commands,
    /// outside scratch.
    planted: [String; 2],
    /// The relative targets those links carry.
    to: [String; 2],
}

fn prepared(run: &str, link_in_scratch: bool) -> Prepared {
    let fx = Fixture::new(run);
    let data = fx.tmp.path().join("homes/.transcript-store");
    fs::create_dir_all(&data).unwrap();
    fs::write(data.join("state.db"), "secret\n").unwrap();
    fx.install_with(WorkerKind::Standard, |config| config.data_dir = Some(data.clone()));
    let dir = fx.process_home.join(".gemini/antigravity-cli/brain").join(CONVERSATION);
    fs::create_dir_all(dir.join("scratch")).unwrap();
    fs::create_dir_all(dir.join(".system_generated/steps")).unwrap();
    fs::create_dir_all(dir.join(".system_generated/tasks")).unwrap();
    fs::write(dir.join("scratch/split_modules.py"), "print('split')\n").unwrap();
    let scratch = dir.join("scratch");
    let hooks = fx.process_home.join(".gemini/config/hooks.json");
    let to_data = relative_from(&scratch, &data);
    let to_hooks = relative_from(&scratch, &hooks);
    let planted_data = prepare_link_via_command(&fx, "p", &to_data);
    let planted_hooks = prepare_link_via_command(&fx, "ph", &to_hooks);
    if link_in_scratch {
        let extra = prepare_link_via_command(&fx, "p-in-scratch", &to_data);
        run_approved(&fx, &format!("mv {} {}/up", extra.display(), scratch.display()));
    }
    Prepared {
        scratch: scratch.display().to_string(),
        planted: [planted_data.display().to_string(), planted_hooks.display().to_string()],
        to: [to_data.display().to_string(), to_hooks.display().to_string()],
        fx,
    }
}

fn deny(fx: &Fixture, command: &str) {
    let payload = command_in(fx, Some(CONVERSATION), command);
    assert_denied(fx, &payload, command);
}

/// Python source uses double quotes only, so the `-c` form is the body
/// unchanged inside single quotes.
fn both_forms(fx: &Fixture, body: &str) {
    assert!(!body.contains('\''), "{body}");
    deny(fx, &format!("python3 -c '{body}'"));
    deny(fx, &format!("python3 - <<'PY'\n{body}\nPY"));
}

#[test]
fn a_body_that_moves_a_prepared_link_into_scratch_is_denied() {
    let p = prepared("run-py-move", false);
    let s = &p.scratch;
    for (source, name, tail) in [(&p.planted[0], "up", "/state.db"), (&p.planted[1], "hooks.json", "")] {
        let dest = format!("{s}/{name}");
        let read = format!("print(open(\"{dest}{tail}\").read())");
        for mover in [
            format!("import os; os.rename(\"{source}\", \"{dest}\")"),
            format!("import os; os.replace(\"{source}\", \"{dest}\")"),
            format!("import shutil; shutil.move(\"{source}\", \"{dest}\")"),
            format!("import shutil; shutil.copytree(\"{source}\", \"{dest}\", symlinks=True)"),
            format!("import pathlib; pathlib.Path(\"{source}\").rename(\"{dest}\")"),
            format!("import pathlib; pathlib.Path(\"{source}\").replace(\"{dest}\")"),
            format!("import pathlib; pathlib.Path(\"{source}\").replace(\"{dest}\",)"),
            format!("import subprocess; subprocess.run([\"mv\", \"{source}\", \"{dest}\"])"),
            format!("import os; os.system(\"mv {source} {dest}\")"),
            format!("__import__(\"os\").rename(\"{source}\", \"{dest}\")"),
            format!("import os; getattr(os, \"rename\")(\"{source}\", \"{dest}\")"),
            format!("import os; m = os.rename; m(\"{source}\", \"{dest}\")"),
            format!("from os import rename as r; r(\"{source}\", \"{dest}\")"),
            format!("import os; os.__dict__[\"rename\"](\"{source}\", \"{dest}\")"),
            format!("import os; vars(os)[\"rename\"](\"{source}\", \"{dest}\")"),
            format!("import tarfile; tarfile.open(\"/tmp/x.tar\").extractall(\"{s}\")"),
        ] {
            both_forms(&p.fx, &format!("{mover}; {read}"));
            // Two bodies in one command: neither is trusted with scratch.
            if mover.contains("os.rename(") || mover.contains("m(") {
                deny(&p.fx, &format!("python3 -c '{mover}'; python3 -c '{read}'"));
                deny(&p.fx, &format!("python3 -c '{mover}' && python3 - <<'PY'\n{read}\nPY"));
            }
        }
    }
}

#[test]
fn a_body_that_plants_a_link_in_scratch_itself_is_denied() {
    let p = prepared("run-py-symlink", false);
    let s = &p.scratch;
    for (to, name, tail) in [(&p.to[0], "up", "/state.db"), (&p.to[1], "hooks.json", "")] {
        let read = format!("print(open(\"{s}/{name}{tail}\").read())");
        for planter in [
            format!("import os; os.symlink(\"{to}\", \"{s}/{name}\")"),
            format!("import os; sl = os.symlink; sl(\"{to}\", \"{s}/{name}\")"),
            format!("from os import symlink as sl; sl(\"{to}\", \"{s}/{name}\")"),
            format!("import pathlib; pathlib.Path(\"{s}/{name}\").symlink_to(\"{to}\")"),
        ] {
            both_forms(&p.fx, &format!("{planter}; {read}"));
        }
    }
}

#[test]
fn a_body_building_the_scratch_path_from_home_at_run_time_is_denied() {
    let p = prepared("run-py-home", true);
    let tail = format!(".gemini/antigravity-cli/brain/{CONVERSATION}/scratch/up/state.db");
    for body in [
        format!("import os; print(open(os.environ[\"HOME\"] + \"/{tail}\").read())"),
        format!("import os; print(open(os.path.expanduser(\"~\") + \"/{tail}\").read())"),
        "import pathlib; print((pathlib.Path.home() / \".gemini\" / \"antigravity-cli\" / \"brain\" / \"c1\" / \"scratch\" / \"up\" / \"state.db\").read_text())".to_owned(),
        "import os; print(open(os.getenv(\"HOME\") + \"/.gemini/config/hooks.json\").read())".to_owned(),
    ] {
        both_forms(&p.fx, &body);
    }
}

#[test]
fn ordinary_scratch_reads_and_string_methods_stay_allowed() {
    let p = prepared("run-py-ok", false);
    let s = &p.scratch;
    for command in [
        format!("python3 -c \"print(open('{s}/split_modules.py').read())\""),
        format!("python3 -c \"print(open('{s}/split_modules.py').read().upper())\""),
    ] {
        assert_allowed(&p.fx, &command_in(&p.fx, Some(CONVERSATION), &command), &command);
    }
}

#[test]
fn a_scratch_link_into_the_workspace_does_not_deny_unrelated_python() {
    let p = prepared("run-py-ws-link", false);
    let target = p.fx.workspace().join("notes.txt");
    fs::write(&target, "ok\n").unwrap();
    std::os::unix::fs::symlink(&target, std::path::Path::new(&p.scratch).join("ws-link")).unwrap();
    for command in ["python3 -c 'print(1)'", "python3 -c 'import os; print(os.getcwd())'"] {
        assert_allowed(&p.fx, &command_in(&p.fx, Some(CONVERSATION), command), command);
    }
    // A link that leaves both scratch and the workspace still denies.
    std::os::unix::fs::symlink("/etc/hosts", std::path::Path::new(&p.scratch).join("out-link")).unwrap();
    deny(&p.fx, "python3 -c 'print(1)'");
}
