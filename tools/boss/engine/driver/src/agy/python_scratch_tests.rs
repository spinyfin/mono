//! A Python body that names this run's scratch must not be able to rearrange
//! scratch and read through the result in the same command, and a path the
//! body builds from the home directory at run time gets no allowance.

use super::scratch_and_quoting_tests::{assert_allowed, assert_denied, command_in};
use super::*;

const CONVERSATION: &str = "c1";

struct Prepared {
    fx: Fixture,
    scratch: String,
    planted: String,
    planted_hooks: String,
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
    let hooks = fx.process_home.join(".gemini/config/hooks.json");
    let planted = fx.tmp.path().join("p");
    let planted_hooks = fx.tmp.path().join("ph");
    std::os::unix::fs::symlink(&data, &planted).unwrap();
    std::os::unix::fs::symlink(&hooks, &planted_hooks).unwrap();
    if link_in_scratch {
        std::os::unix::fs::symlink(&data, dir.join("scratch/up")).unwrap();
    }
    Prepared {
        scratch: dir.join("scratch").display().to_string(),
        planted: planted.display().to_string(),
        planted_hooks: planted_hooks.display().to_string(),
        fx,
    }
}

fn deny(fx: &Fixture, command: &str) {
    let payload = command_in(fx, Some(CONVERSATION), command);
    assert_denied(fx, &payload, command);
}

fn both_forms(fx: &Fixture, body: &str) {
    deny(fx, &format!("python3 -c '{}'", body.replace('\'', "")));
    deny(fx, &format!("python3 - <<'PY'\n{body}\nPY"));
}

#[test]
fn a_body_that_moves_a_prepared_link_into_scratch_is_denied() {
    let p = prepared("run-py-move", false);
    let (s, link, hooks) = (&p.scratch, &p.planted, &p.planted_hooks);
    for (source, name, target) in [(link, "up", "state.db"), (hooks, "hooks.json", "")] {
        let read = format!("print(open('{s}/{name}/{target}').read())");
        for mover in [
            format!("import os; os.rename('{source}', '{s}/{name}')"),
            format!("import os; os.replace('{source}', '{s}/{name}')"),
            format!("import shutil; shutil.move('{source}', '{s}/{name}')"),
            format!("import shutil; shutil.copytree('{source}', '{s}/{name}', symlinks=True)"),
            format!("import pathlib; pathlib.Path('{source}').rename('{s}/{name}')"),
            format!("import pathlib; pathlib.Path('{source}').replace('{s}/{name}')"),
            format!("import subprocess; subprocess.run(['mv', '{source}', '{s}/{name}'])"),
            format!("import os; os.system('mv {source} {s}/{name}')"),
            format!("__import__('os').rename('{source}', '{s}/{name}')"),
            format!("import os; getattr(os, 'rename')('{source}', '{s}/{name}')"),
        ] {
            both_forms(&p.fx, &format!("{mover}; {read}"));
            // Two bodies in one command: neither is trusted with scratch.
            deny(&p.fx, &format!("python3 -c \"{mover}\"; python3 -c \"{read}\""));
            deny(
                &p.fx,
                &format!("python3 -c \"{mover}\" && python3 - <<'PY'\n{read}\nPY"),
            );
        }
    }
}

#[test]
fn a_body_building_the_scratch_path_from_home_at_run_time_is_denied() {
    let p = prepared("run-py-home", true);
    let tail = format!(".gemini/antigravity-cli/brain/{CONVERSATION}/scratch/up/state.db");
    for body in [
        format!("import os; print(open(os.environ['HOME'] + '/{tail}').read())"),
        format!("import os; print(open(os.path.expanduser('~') + '/{tail}').read())"),
        "import pathlib; print((pathlib.Path.home() / '.gemini' / 'antigravity-cli' / 'brain' / 'c1' / 'scratch' / 'up' / 'state.db').read_text())".to_owned(),
        "import os; print(open(os.getenv('HOME') + '/.gemini/config/hooks.json').read())".to_owned(),
    ] {
        both_forms(&p.fx, &body);
    }
}

#[test]
fn ordinary_scratch_reads_and_string_replace_stay_allowed() {
    let p = prepared("run-py-ok", false);
    let s = &p.scratch;
    for command in [
        format!("python3 -c \"print(open('{s}/split_modules.py').read())\""),
        format!("python3 -c \"print(open('{s}/split_modules.py').read().replace('a', 'b'))\""),
    ] {
        assert_allowed(&p.fx, &command_in(&p.fx, Some(CONVERSATION), &command), &command);
    }
}
