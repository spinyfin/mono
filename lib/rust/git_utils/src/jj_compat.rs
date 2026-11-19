//! Compatibility for the jj versions deployed on cube and Boss hosts.
//!
//! Callers keep one command vocabulary: the jj 0.26 spelling. A transport that
//! knows which jj it is about to run (cube's command runner, the engine's
//! local and SSH `Jj` implementations) passes each argument vector through
//! [`arguments`], which respells whatever that version no longer accepts.
//! Nothing is retried and no jj error is swallowed: a removed flag is never
//! sent to a jj that lacks it.
//!
//! The audit behind every rule, and the upgrade notes, are in
//! `tools/cube/docs/jj-compatibility.md`.
use std::collections::HashMap;
use std::ffi::{OsStr, OsString};
use std::process::Command;
use std::sync::{Mutex, OnceLock};

/// Oldest and newest supported jj minor releases (`0.MIN.x` to `0.MAX.x`).
/// Re-run the audit before raising the ceiling: jj removes deprecated flags,
/// templates and config keys in ordinary minor releases.
pub const MIN_MINOR: u32 = 26;
pub const MAX_MINOR: u32 = 46;

/// The minor number of a supported `0.x` jj release.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Version(pub u32);

impl Version {
    /// Parse `jj --version` output, refusing anything outside the supported
    /// range. Release builds print `jj 0.46.0-<commit hash>`; Homebrew prints
    /// `jj 0.26.0`. Pre-release and development builds are refused: their
    /// behaviour is not the audited release's.
    pub fn parse(output: &str) -> Result<Self, String> {
        let unsupported = || {
            format!(
                "Unsupported jj version {:?}: cube and Boss support jj 0.{MIN_MINOR}.x through 0.{MAX_MINOR}.x. \
                 Install a supported release (for example `brew install jj`, or a pinned release from \
                 https://github.com/jj-vcs/jj/releases) and make sure it is the `jj` on PATH. \
                 To support a newer jj, audit it against tools/cube/docs/jj-compatibility.md first.",
                output.trim()
            )
        };
        let version = output.trim().strip_prefix("jj ").ok_or_else(unsupported)?;
        let version = match version.split_once('-') {
            Some((release, hash)) if hash.len() >= 12 && hash.bytes().all(|b| b.is_ascii_hexdigit()) => release,
            Some(_) => return Err(unsupported()),
            None => version,
        };
        let mut parts = version.split('.');
        let (Some(major), Some(minor), Some(patch), None) = (parts.next(), parts.next(), parts.next(), parts.next())
        else {
            return Err(unsupported());
        };
        match (major, minor.parse::<u32>(), patch.parse::<u32>()) {
            ("0", Ok(minor), Ok(_)) if (MIN_MINOR..=MAX_MINOR).contains(&minor) => Ok(Self(minor)),
            _ => Err(unsupported()),
        }
    }
}

/// The supported version of `binary`, probed once per executable per process.
///
/// The probe runs from the temp directory, never the caller's directory: from
/// 0.38 jj migrates the configuration of whichever repository it finds from
/// its working directory, even for `--version`.
pub fn detected_version(binary: &OsStr) -> Result<Version, String> {
    static VERSIONS: OnceLock<Mutex<HashMap<OsString, Version>>> = OnceLock::new();
    let mut versions = VERSIONS
        .get_or_init(Default::default)
        .lock()
        .map_err(|e| e.to_string())?;
    if let Some(version) = versions.get(binary) {
        return Ok(*version);
    }
    // A relative path names a file under the caller's directory, not the temp
    // directory the probe runs in. A bare name is resolved through PATH.
    let executable = if std::path::Path::new(binary).components().count() > 1 {
        std::fs::canonicalize(binary)
            .map_err(|e| format!("Cannot locate jj at {binary:?}: {e}"))?
            .into_os_string()
    } else {
        binary.to_owned()
    };
    let output = Command::new(&executable)
        .arg("--version")
        .current_dir(std::env::temp_dir())
        .output()
        .map_err(|e| {
            format!(
                "Cannot run `{} --version`: {e}. Install jj 0.{MIN_MINOR}.x through 0.{MAX_MINOR}.x on PATH.",
                binary.to_string_lossy()
            )
        })?;
    if !output.status.success() {
        return Err(format!(
            "`{} --version` failed: {}",
            binary.to_string_lossy(),
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    let version = Version::parse(&String::from_utf8_lossy(&output.stdout))?;
    versions.insert(binary.to_owned(), version);
    Ok(version)
}

/// Global options that take a value, so the word after them is not the command.
const GLOBAL_VALUE_FLAGS: [&str; 7] = [
    "-R",
    "--repository",
    "--at-op",
    "--at-operation",
    "--config",
    "--config-file",
    "--color",
];

/// Indexes of the command word and the word after it (`git` and `push`).
fn command_words(args: &[String]) -> (Option<usize>, Option<usize>) {
    let mut index = 0;
    while index < args.len() {
        let arg = args[index].as_str();
        if GLOBAL_VALUE_FLAGS.contains(&arg) {
            index += 2;
        } else if arg.starts_with('-') {
            index += 1;
        } else {
            return (Some(index), (index + 1 < args.len()).then_some(index + 1));
        }
    }
    (None, None)
}

/// The command that must run before `args` (written for jj 0.26) on `version`
/// to keep their meaning, as its arguments, or `None` when none is needed.
///
/// `git push --allow-new` has no spelling on 0.42 and later, and 0.38 and later
/// refuse a new bookmark that is already tracked on another remote, so dropping
/// the flag alone does not preserve it: the destination bookmark is tracked
/// first. [`arguments`] stays a pure respelling; a consumer runs this command
/// itself, before the push, so it is visible and bounded like any other. Both
/// `--flag value` and `--flag=value` spellings of the bookmark and remote are
/// read; a push that names no bookmark or no remote needs no tracking.
pub fn preparatory_arguments(version: Version, args: &[String]) -> Option<Vec<String>> {
    let (command, sub) = command_words(args);
    let is = |index: Option<usize>, word: &str| index.is_some_and(|i| args[i] == word);
    if version.0 < 38 || !is(command, "git") || !is(sub, "push") || !args.iter().any(|arg| arg == "--allow-new") {
        return None;
    }
    let value = |flags: &[&str]| {
        args.iter().enumerate().find_map(|(index, arg)| {
            flags.iter().find_map(|flag| {
                if arg == flag {
                    args.get(index + 1).map(String::as_str)
                } else {
                    arg.strip_prefix(flag)?.strip_prefix('=')
                }
            })
        })
    };
    let (bookmark, remote) = (value(&["-b", "--bookmark"])?, value(&["--remote"])?);
    Some(vec![
        "bookmark".to_owned(),
        "track".to_owned(),
        format!("{bookmark}@{remote}"),
        "--ignore-working-copy".to_owned(),
    ])
}

/// Respell `args`, written for jj 0.26, for the jj that is about to run them.
pub fn arguments(version: Version, args: &[String]) -> Vec<String> {
    let (command, sub) = command_words(args);
    let word = |index: Option<usize>| index.map_or("", |i| args[i].as_str());
    let (command_word, sub_word) = (word(command), word(sub));
    let has = |flag: &str| args.iter().any(|arg| arg == flag);
    let mut result = Vec::with_capacity(args.len() + 2);

    // 0.34 colocates new repositories and 0.46 new workspaces by default.
    // Cube's layout is an explicitly colocated store with plain secondary
    // workspaces, so pin the default; `--colocate` on the command still wins.
    // jj before 0.33 ignores the key.
    if matches!(
        (command_word, sub_word),
        ("workspace", "add") | ("git", "init" | "clone")
    ) {
        result.extend(["--config".to_owned(), "git.colocate=false".to_owned()]);
    }
    for (index, arg) in args.iter().enumerate() {
        let previous = index.checked_sub(1).map_or("", |i| args[i].as_str());
        match (command_word, sub_word) {
            // 0.38 tracks a new bookmark named with `-b` by itself; 0.42
            // removed the flag.
            ("git", "push") if version.0 >= 38 && arg == "--allow-new" => {}
            // 0.35 removed the flag. It only suppressed a listing.
            ("abandon", _) if version.0 >= 35 && arg == "--summary" => {}
            // From 0.36 a bare path is a glob, so `a[1]` matches nothing and
            // jj exits zero. Callers name one file or directory.
            ("file", "show" | "list")
                if version.0 >= 36 && sub.is_some_and(|sub| index > sub) && is_bare_path(arg, previous) =>
            {
                result.push(format!("cwd:{arg:?}"));
            }
            // 0.33 renamed the command; 0.39 removed the old name.
            ("op" | "operation", "undo") if version.0 >= 33 && Some(index) == sub => result.push("revert".to_owned()),
            // 0.33 removed `debug operation`; `debug object operation` prints
            // the same structure.
            ("debug", "operation") if version.0 >= 33 && Some(index) == sub => {
                result.extend(["object".to_owned(), "operation".to_owned()]);
            }
            ("debug", "operation") if version.0 >= 33 && (arg == "--display" || previous == "--display") => {}
            // 0.33 templates an evolution entry, not a commit.
            ("evolog", _) if version.0 >= 33 && matches!(previous, "-T" | "--template") => {
                result.push(evolution_template(arg));
            }
            // From 0.32 a change ID symbol is an error when the change is
            // divergent; `change_id()` (0.31) selects every visible commit of
            // the change, which is what the symbol meant before.
            ("log", _) if version.0 >= 31 && matches!(previous, "-r" | "--revisions") => {
                result.push(select_change_ids(arg));
            }
            _ => result.push(arg.clone()),
        }
    }
    // 0.27 made `forget` keep (untracked) remote bookmarks. Callers forget a
    // bookmark to drop every record of it, as 0.26 did.
    if (command_word, sub_word) == ("bookmark", "forget") && version.0 >= 27 && !has("--include-remotes") {
        result.push("--include-remotes".to_owned());
    }
    result
}

/// Whether `arg` of `file show` or `file list` is a path with no fileset
/// syntax of its own: not a flag, not a flag's value, no `kind:` prefix.
fn is_bare_path(arg: &str, previous: &str) -> bool {
    let kind = arg
        .split_once(':')
        .is_some_and(|(kind, _)| !kind.is_empty() && kind.bytes().all(|b| b.is_ascii_lowercase() || b == b'-'));
    !arg.starts_with('-') && !kind && !matches!(previous, "-r" | "--revision" | "-T" | "--template")
}

/// `commit_id` as a bare keyword becomes `commit.commit_id()`.
fn evolution_template(template: &str) -> String {
    const KEYWORD: &str = "commit_id";
    let mut result = String::with_capacity(template.len() + 16);
    let mut rest = template;
    while let Some(start) = rest.find(KEYWORD) {
        let end = start + KEYWORD.len();
        let is_word = |c: char| c.is_ascii_alphanumeric() || c == '_';
        let bare = !rest[..start].ends_with(|c: char| is_word(c) || c == '.')
            && !rest[end..].starts_with(|c: char| is_word(c) || c == '(');
        result.push_str(&rest[..start]);
        result.push_str(if bare { "commit.commit_id()" } else { KEYWORD });
        rest = &rest[end..];
    }
    result.push_str(rest);
    result
}

/// Wrap every full-length change ID in `revset` in `change_id()`.
///
/// A change ID is 32 characters of `k` to `z`. No bookmark, tag or function
/// cube or Boss uses has that shape, and a shorter prefix is left alone.
fn select_change_ids(revset: &str) -> String {
    const LENGTH: usize = 32;
    let is_word = |c: char| c.is_ascii_alphanumeric() || matches!(c, '_' | '/' | '.' | '-' | '+' | '@');
    let mut result = String::with_capacity(revset.len() + 16);
    let mut quoted = false;
    let mut word_start = None;
    for (index, c) in revset.char_indices().chain([(revset.len(), ' ')]) {
        if !quoted && is_word(c) {
            word_start.get_or_insert(index);
            continue;
        }
        if let Some(start) = word_start.take() {
            let word = &revset[start..index];
            let change_id = word.len() == LENGTH && word.bytes().all(|b| (b'k'..=b'z').contains(&b));
            if change_id && !result.ends_with("change_id(") && c != '(' {
                result.push_str(&format!("change_id({word})"));
            } else {
                result.push_str(word);
            }
        }
        if index < revset.len() {
            if c == '"' {
                quoted = !quoted;
            }
            result.push(c);
        }
    }
    result
}

/// The error a successful command must be reported as, if any.
///
/// From 0.36 a name that matches nothing is a warning and the command exits
/// zero. Callers of these two commands rely on the failure: cube probes for
/// the default branch with `bookmark track`, and a push that pushed nothing
/// must not be reported as published.
pub fn unmatched_name_error(version: Version, args: &[String], stderr: &str) -> Option<String> {
    if version.0 < 36 {
        return None;
    }
    let (command, sub) = command_words(args);
    let word = |index: Option<usize>| index.map_or("", |i| args[i].as_str());
    let names = |marker: &str| {
        stderr
            .lines()
            .find_map(|line| line.split_once(marker).map(|(_, names)| names.trim().to_owned()))
    };
    match (word(command), word(sub)) {
        ("bookmark", "track") => names("No matching remote bookmarks for names:")
            .map(|names| format!("Error: No such remote bookmark: {names}")),
        ("git", "push") => {
            names("No matching bookmarks for names:").map(|names| format!("Error: No such bookmark: {names}"))
        }
        _ => None,
    }
}

/// The file patches of `jj interdiff --git` output.
///
/// From 0.33 a difference in descriptions is printed first, as a patch to
/// `JJ-COMMIT-DESCRIPTION` with no `index` or mode line. A patch to a real
/// file of that name has those lines and is kept.
pub fn interdiff_files(version: Version, output: &str) -> &str {
    const DESCRIPTION: &str = "diff --git a/JJ-COMMIT-DESCRIPTION b/JJ-COMMIT-DESCRIPTION\n--- JJ-COMMIT-DESCRIPTION\n";
    if version.0 >= 33 && output.starts_with(DESCRIPTION) {
        output.find("\ndiff --git ").map_or("", |start| &output[start + 1..])
    } else {
        output
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn adapt(version: u32, args: &[&str]) -> Vec<String> {
        arguments(
            Version(version),
            &args.iter().map(|arg| (*arg).to_owned()).collect::<Vec<_>>(),
        )
    }

    #[test]
    fn version_bounds_fail_closed_with_an_actionable_message() {
        for (output, minor) in [
            ("jj 0.26.0", 26),
            ("jj 0.46.0\n", 46),
            ("jj 0.46.1", 46),
            ("jj 0.46.0-7d382314f9420b4b8d932245b49c99492c812b9f", 46),
            ("jj 0.33.0", 33),
        ] {
            assert_eq!(Version::parse(output), Ok(Version(minor)), "{output}");
        }
        for output in [
            "jj 0.25.0",
            "jj 0.47.0",
            "jj 1.0.0",
            "jj 0.46",
            "jj 0.46.0.1",
            "jj 0.46.0-dev",
            "git version 2.48.1",
            "",
        ] {
            let error = Version::parse(output).unwrap_err();
            assert!(error.contains("jj 0.26.x through 0.46.x"), "{error}");
            assert!(error.contains("Install a supported release"), "{error}");
        }
    }

    #[test]
    fn old_jj_receives_the_arguments_unchanged() {
        for args in [
            &["git", "push", "-b", "feature", "--remote", "origin", "--allow-new"][..],
            &["op", "undo", "abc"],
            &[
                "--ignore-working-copy",
                "evolog",
                "--no-graph",
                "-r",
                "x",
                "-T",
                "commit_id ++ \"\\n\"",
            ],
            &[
                "log",
                "-r",
                "present(kkkkllllmmmmnnnnooooppppqqqqrrrr)",
                "-T",
                "commit_id",
            ],
            &["--ignore-working-copy", "bookmark", "forget", "exact:a"],
            &[
                "--at-operation",
                "o",
                "debug",
                "operation",
                "--display",
                "operation",
                "o",
            ],
            &["--ignore-immutable", "abandon", "--summary", "x"],
            &["file", "show", "-r", "x", ".cube/a[1].sh"],
        ] {
            assert_eq!(adapt(26, args), args);
        }
    }

    #[test]
    fn paths_stay_literal_and_removed_flags_are_dropped() {
        assert_eq!(
            adapt(
                46,
                &[
                    "--ignore-working-copy",
                    "--ignore-immutable",
                    "abandon",
                    "--summary",
                    "x"
                ]
            ),
            ["--ignore-working-copy", "--ignore-immutable", "abandon", "x"]
        );
        assert_eq!(adapt(34, &["abandon", "--summary", "x"]), ["abandon", "--summary", "x"]);
        assert_eq!(
            adapt(
                46,
                &["--ignore-working-copy", "file", "show", "-r", "main", ".cube/a[1].sh"]
            ),
            [
                "--ignore-working-copy",
                "file",
                "show",
                "-r",
                "main",
                "cwd:\".cube/a[1].sh\""
            ]
        );
        assert_eq!(
            adapt(46, &["file", "list", "-r", "@", ".cube", "-T", "path ++ \"\\n\""]),
            ["file", "list", "-r", "@", "cwd:\".cube\"", "-T", "path ++ \"\\n\""]
        );
        assert_eq!(
            adapt(46, &["file", "show", "-r", "main", "--", "Cargo.lock"]),
            ["file", "show", "-r", "main", "--", "cwd:\"Cargo.lock\""]
        );
        // Already a fileset, or not a path at all.
        assert_eq!(adapt(46, &["file", "show", "cwd:\"a\""]), ["file", "show", "cwd:\"a\""]);
        assert_eq!(adapt(46, &["file", "show", "glob:*.rs"]), ["file", "show", "glob:*.rs"]);
        assert_eq!(adapt(46, &["file", "track", "a[1]"]), ["file", "track", "a[1]"]);
    }

    #[test]
    fn a_new_destination_is_tracked_before_the_push_in_every_flag_spelling() {
        let track = |version, args: &[&str]| {
            preparatory_arguments(
                Version(version),
                &args.iter().map(|arg| (*arg).to_owned()).collect::<Vec<_>>(),
            )
        };
        let expected = Some(vec![
            "bookmark".to_owned(),
            "track".to_owned(),
            "feat@github".to_owned(),
            "--ignore-working-copy".to_owned(),
        ]);
        for args in [
            &["git", "push", "-b", "feat", "--remote", "github", "--allow-new"][..],
            &[
                "--ignore-working-copy",
                "git",
                "push",
                "--bookmark",
                "feat",
                "--remote",
                "github",
                "--allow-new",
            ][..],
            &["git", "push", "--bookmark=feat", "--remote=github", "--allow-new"][..],
        ] {
            assert_eq!(track(46, args), expected, "{args:?}");
            assert_eq!(track(38, args), expected, "{args:?}");
            assert_eq!(track(37, args), None, "{args:?}");
        }
        // Nothing to track without the flag, a bookmark or a remote, and only
        // for a push.
        assert_eq!(track(46, &["git", "push", "-b", "feat", "--remote", "github"]), None);
        assert_eq!(track(46, &["git", "push", "-b", "feat", "--allow-new"]), None);
        assert_eq!(track(46, &["git", "push", "--remote", "github", "--allow-new"]), None);
        assert_eq!(
            track(46, &["log", "-b", "feat", "--remote", "github", "--allow-new"]),
            None
        );
    }

    #[test]
    fn removed_spellings_are_never_sent_to_new_jj() {
        assert_eq!(
            adapt(
                46,
                &["git", "push", "-b", "feature", "--remote", "origin", "--allow-new"]
            ),
            ["git", "push", "-b", "feature", "--remote", "origin"]
        );
        assert_eq!(adapt(37, &["git", "push", "-b", "f", "--allow-new"]).len(), 5);
        assert_eq!(
            adapt(46, &["-R", "undo", "op", "undo", "abc"]),
            ["-R", "undo", "op", "revert", "abc"]
        );
        // The bookmark is named `undo`; only the command word is respelled.
        assert_eq!(
            adapt(46, &["bookmark", "set", "undo", "-r", "@"]),
            ["bookmark", "set", "undo", "-r", "@"]
        );
        assert_eq!(
            adapt(
                46,
                &[
                    "--ignore-working-copy",
                    "--at-operation",
                    "o",
                    "debug",
                    "operation",
                    "--display",
                    "operation",
                    "o"
                ]
            ),
            [
                "--ignore-working-copy",
                "--at-operation",
                "o",
                "debug",
                "object",
                "operation",
                "o"
            ]
        );
        assert_eq!(
            adapt(
                46,
                &["--ignore-working-copy", "bookmark", "forget", "exact:a", "exact:b"]
            ),
            [
                "--ignore-working-copy",
                "bookmark",
                "forget",
                "exact:a",
                "exact:b",
                "--include-remotes"
            ]
        );
    }

    #[test]
    fn evolution_templates_read_the_commit_of_the_entry() {
        assert_eq!(
            adapt(
                46,
                &["evolog", "--no-graph", "-r", "commit_id", "-T", "commit_id ++ \"\\n\""]
            ),
            [
                "evolog",
                "--no-graph",
                "-r",
                "commit_id",
                "-T",
                "commit.commit_id() ++ \"\\n\""
            ]
        );
        assert_eq!(
            evolution_template("commit.commit_id() ++ my_commit_id ++ commit_id.short()"),
            "commit.commit_id() ++ my_commit_id ++ commit.commit_id().short()"
        );
        // A log template is a commit template on every version.
        assert_eq!(adapt(46, &["log", "-T", "commit_id"]), ["log", "-T", "commit_id"]);
    }

    #[test]
    fn full_change_ids_select_every_visible_commit_of_the_change() {
        let id = "kkkkllllmmmmnnnnooooppppqqqqrrrr";
        let wrapped = format!("change_id({id})");
        for (revset, expected) in [
            (
                format!("present({id}) & ~root()"),
                format!("present({wrapped}) & ~root()"),
            ),
            (format!("{id} & abc123::"), format!("{wrapped} & abc123::")),
            (format!("({id}|{id})"), format!("({wrapped}|{wrapped})")),
            (wrapped.clone(), wrapped.clone()),
            // Not a change ID: a prefix, a bookmark, a quoted name, a commit.
            ("kkkkllllmmmm".to_owned(), "kkkkllllmmmm".to_owned()),
            (format!("boss/{id}"), format!("boss/{id}")),
            (
                format!("bookmarks(exact:\"{id}\")"),
                format!("bookmarks(exact:\"{id}\")"),
            ),
            (format!("{id}@origin"), format!("{id}@origin")),
            (
                "0123456789abcdef0123456789abcdef".to_owned(),
                "0123456789abcdef0123456789abcdef".to_owned(),
            ),
        ] {
            assert_eq!(adapt(46, &["log", "--no-graph", "-r", &revset, "-T", id])[3], expected);
            assert_eq!(adapt(30, &["log", "--no-graph", "-r", &revset])[3], revset);
        }
        // Only queries: a command that needs one revision keeps the symbol
        // and jj's own divergence error.
        assert_eq!(adapt(46, &["new", id]), ["new", id]);
        assert_eq!(adapt(46, &["edit", "-r", id]), ["edit", "-r", id]);
    }

    #[test]
    fn new_repositories_and_workspaces_keep_the_cube_layout() {
        for version in [26, 46] {
            assert_eq!(
                adapt(version, &["-R", "store", "workspace", "add", "--name", "w", "path"]),
                [
                    "--config",
                    "git.colocate=false",
                    "-R",
                    "store",
                    "workspace",
                    "add",
                    "--name",
                    "w",
                    "path"
                ]
            );
            assert_eq!(
                adapt(version, &["git", "init", "--colocate"])[..2],
                ["--config", "git.colocate=false"]
            );
            assert_eq!(adapt(version, &["git", "clone", "--colocate", "u", "p"]).len(), 7);
            assert_eq!(
                adapt(version, &["workspace", "forget", "add"]),
                ["workspace", "forget", "add"]
            );
            assert_eq!(adapt(version, &["git", "fetch"]), ["git", "fetch"]);
        }
    }

    #[test]
    fn unmatched_names_stay_errors() {
        let args = |args: &[&str]| args.iter().map(|arg| (*arg).to_owned()).collect::<Vec<_>>();
        let track = args(&["bookmark", "track", "master@origin"]);
        let warning = "Warning: No matching remote bookmarks for names: master@origin\nNothing changed.\n";
        assert_eq!(
            unmatched_name_error(Version(46), &track, warning).as_deref(),
            Some("Error: No such remote bookmark: master@origin")
        );
        assert_eq!(unmatched_name_error(Version(26), &track, warning), None);
        assert_eq!(
            unmatched_name_error(Version(46), &track, "Started tracking 1 remote bookmarks.\n"),
            None
        );
        let push = args(&["git", "push", "-b", "gone", "--remote", "origin"]);
        assert_eq!(
            unmatched_name_error(Version(46), &push, "Warning: No matching bookmarks for names: gone\n").as_deref(),
            Some("Error: No such bookmark: gone")
        );
        // Removing something that is already gone is the wanted end state.
        let forget = args(&["bookmark", "forget", "gone"]);
        assert_eq!(
            unmatched_name_error(Version(46), &forget, "Warning: No matching bookmarks for names: gone\n"),
            None
        );
    }

    #[test]
    fn description_patch_is_not_a_file_patch() {
        let metadata = "diff --git a/JJ-COMMIT-DESCRIPTION b/JJ-COMMIT-DESCRIPTION\n--- JJ-COMMIT-DESCRIPTION\n+++ JJ-COMMIT-DESCRIPTION\n@@ -1,1 +1,1 @@\n-old\n+new\n";
        let file = "diff --git a/JJ-COMMIT-DESCRIPTION b/JJ-COMMIT-DESCRIPTION\nindex 123..456 100644\n--- a/JJ-COMMIT-DESCRIPTION\n+++ b/JJ-COMMIT-DESCRIPTION\n";
        assert_eq!(interdiff_files(Version(46), metadata), "");
        assert_eq!(interdiff_files(Version(46), file), file);
        assert_eq!(interdiff_files(Version(46), &format!("{metadata}{file}")), file);
        assert_eq!(interdiff_files(Version(26), metadata), metadata);
    }
}
