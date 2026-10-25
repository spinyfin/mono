//! Shared-source-store write gate: the `PreToolUse` guard that keeps raw
//! `git` writes out of cube's shared jj/git store.
//!
//! # Why this exists
//!
//! Every cube workspace is a *secondary* jj workspace: its `.jj/repo` is a
//! pointer file naming the one object store all leased workspaces of a
//! repository share (`<cube repos root>/<repo>/.jj/repo`, next to a real,
//! non-bare `.git`). Plain `git` run inside a workspace fails with "not a git
//! repository", so workers that wanted raw git reached the store directly in
//! two observed ways: exporting `GIT_DIR=<repos root>/<repo>/.git`, or
//! `cd`-ing into the store checkout itself. One such command — a
//! `git fetch --depth=200` of the kind a repository's own CI script runs —
//! turned the shared store into a shallow clone, after which every dispatch
//! for that repository failed for days because recovery-bookmark creation hit
//! a commit beyond the shallow boundary.
//!
//! The store is engine/cube-owned; the only sanctioned write paths are jj
//! inside the worker's own workspace and the `cube` helpers. This guard makes
//! that a control rather than a convention. It is one script shared by all
//! three drivers (Claude installs it as a settings-file hook, Codex
//! materialises it into the attested `CODEX_HOME/guards/` chain, Grok wraps
//! it in its hook adapter), the same way the Boss-data-dir path guard is.
//!
//! # What it refuses
//!
//! - Any `git` invocation whose repository, work tree or working directory
//!   resolves under cube's repos root, unless the subcommand is clearly
//!   read-only (`log`, `show`, `diff`, `rev-parse`, `cat-file`, `ls-files`,
//!   `merge-base`, `status`, `blame`, …; `branch`/`tag`/`config`/`remote`/
//!   `stash`/`reflog` only in their listing forms). The repository is located
//!   the way git itself locates it: `--git-dir`/`--work-tree`, a `GIT_DIR` /
//!   `GIT_WORK_TREE` assignment on the command, an `export` earlier in the same
//!   command, the hook's inherited environment, `-C`, a preceding `cd`, and
//!   finally the tool call's working directory. Single-level shell variables
//!   are resolved so `S=<store>; git -C "$S" fetch` is seen for what it is;
//!   `sh -c '…'`/`bash -c '…'`/`eval` bodies are inspected recursively.
//! - Any `fetch`/`pull`/`clone` carrying a shallow option (`--depth`,
//!   `--shallow-since`, `--shallow-exclude`, `--update-shallow`, `--deepen`,
//!   `--unshallow`) that would touch the store — with its own message naming
//!   the incident shape, because that is the one that took the fleet down.
//! - Scripts or other programs with a shared-store git environment, and
//!   non-reader programs launched from the shared store.
//! - A mutating `git` whose target cannot be resolved (command substitution,
//!   an unknown variable) when the command text names the repos root anyway.
//!
//! Default-deny: a subcommand the guard does not recognise as read-only is a
//! write. Git aliases cannot shadow builtins, so an unknown name is either a
//! typo or an alias — neither is something a worker should run against the
//! shared store.
//!
//! # What it approves
//!
//! Read-only git against the store (including `git --git-dir <store>/.git
//! log`, which is the sanctioned explicit form), every `git` whose repository
//! resolves elsewhere (a throwaway clone under the worker's temp dir, which is
//! what the refusal tells the worker to use for CI-script reproduction),
//! `jj` (jj's own fetch is how the store is meant to be synced), and every
//! non-shell tool call. Like the other Boss guards it fails *closed* on a
//! payload it cannot read, and it never rewrites a command: the worker gets a
//! loud refusal that says what to do instead.
//!
//! The repos root is derived at decision time from the workspace's `.jj/repo`
//! pointer (`BOSS_WORKSPACE` on the hook command, else the call's cwd, walking
//! up to the pointer). A workspace with no pointer is not a cube lease — there
//! is no shared store to protect, so the guard has nothing to say.

/// Filename the guard is materialised under. Stable so hook commands can be
/// recognised by it (`… python3 <dir>/boss-git-store-guard.py`); the
/// containing directory is content-addressed by the engine.
pub const GIT_STORE_GUARD_SCRIPT_NAME: &str = "boss-git-store-guard.py";

/// Hook-command environment variable naming the leased workspace, from
/// which the guard derives cube's repos root. Falls back to the tool call's
/// working directory when absent.
pub const GIT_STORE_GUARD_WORKSPACE_ENV: &str = "BOSS_WORKSPACE";

/// The hook command Claude's settings file and Grok's adapter run for this
/// guard: `BOSS_WORKSPACE='<workspace>' python3 '<script>'`. The workspace is
/// omitted when the caller does not know it; the script then derives the
/// repos root from the tool call's cwd instead.
pub fn hook_command(script: &std::path::Path, workspace: Option<&std::path::Path>) -> String {
    let script = boss_ssh_transport::shell_quote(&script.display().to_string());
    match workspace {
        Some(workspace) => format!(
            "{GIT_STORE_GUARD_WORKSPACE_ENV}={} python3 {script}",
            boss_ssh_transport::shell_quote(&workspace.display().to_string()),
        ),
        None => format!("python3 {script}"),
    }
}

/// The guard script. Rendered with the shared review-guide submission
/// masker so a proven literal guide body is never lexed as shell.
pub const GIT_STORE_GUARD_SCRIPT: &str = crate::render_review_guide_guard!(
    r##"#!/usr/bin/env python3
"""Shared-source-store write gate (PreToolUse hook).

Refuses any shell command that would run git with write intent against a path
under cube's shared repos root -- the one jj/git object store every leased
workspace of a repository shares -- whether git finds that store through the
working directory, a preceding cd, -C, GIT_DIR / GIT_WORK_TREE, --git-dir or
--work-tree. Read-only git (log, show, diff, rev-parse, cat-file, ls-files,
merge-base, ...) against the store is allowed. A shallow fetch, pull or clone
(--depth, --shallow-since, --shallow-exclude, --update-shallow, --deepen,
--unshallow) that would touch the store is always refused, with a message
naming that specific hazard.

The repos root is derived from the leased workspace's .jj/repo pointer file
(BOSS_WORKSPACE on the hook command, else the call's cwd, walking upwards). A
workspace without that pointer is not a cube lease: nothing to protect, so the
gate approves.

The PreToolUse payload arrives as JSON on stdin; a decision JSON is written to
stdout. Fails CLOSED on a payload it cannot read.
"""
import json
import os
import re
import shlex
import sys

"##,
    crate::render_command_tokenizer!(
        "",
        r##"

MALFORMED = (
    "Blocked (fail-closed): the shared-source-store gate could not read this "
    "tool call's payload, so it cannot tell whether the call runs git against "
    "cube's shared repository store. Guards deny what they cannot parse rather "
    "than approving by default. Re-issue the work as an ordinary shell command, "
    "and report this payload shape to the repo owner -- it means guard wiring "
    "needs updating for this agent driver."
)

WRITE_RECOVERY = (
    "Blocked: this command would run `git %(sub)s` against cube's shared "
    "source store under %(root)s (target: %(target)s). That store is the single "
    "jj/git object store every leased workspace of the repository shares. A raw "
    "git write there -- fetch, pull, clone, checkout, reset, gc, branch or tag "
    "changes, config edits -- can corrupt or shallow it for every workspace and "
    "every future dispatch, and nothing repairs it automatically. Do not point "
    "git at the store with GIT_DIR, --git-dir, --work-tree, -C or cd. Instead: do "
    "VCS work with jj inside your own workspace (`jj git fetch`, `jj new`, "
    "`jj describe`, `jj bookmark set`, `jj rebase`); publish with the cube PR "
    "helpers (`\"$CUBE_BIN\" pr create` / `\"$CUBE_BIN\" pr update`); and to "
    "reproduce a CI script that runs raw git, make a throwaway clone under your "
    "own temp directory and run it there (for example "
    "`git clone <remote URL> \"$TMPDIR/ci-repro\"`; `jj git remote list` prints "
    "the URL). Read-only git against the store -- log, show, diff, rev-parse, "
    "cat-file, ls-files, merge-base and the like -- is still allowed, for "
    "example `git --git-dir %(root)s/<repo>/.git log -1`. Do not work around "
    "this gate."
)

SHALLOW_RECOVERY = (
    "Blocked: `git %(sub)s` with a shallow option (%(flag)s) would touch cube's "
    "shared source store under %(root)s (target: %(target)s). A shallow fetch, "
    "pull or clone into the shared store turns the whole store into a shallow "
    "clone; after that every dispatch for the repository fails on the missing "
    "history until a human repairs the store by hand. Never run a shallow git "
    "operation against the store, from any directory and with any "
    "GIT_DIR/--git-dir/-C setting. To reproduce a CI script that fetches with "
    "--depth, run it in a throwaway clone under your own temp directory "
    "(`git clone <remote URL> \"$TMPDIR/ci-repro\"`; `jj git remote list` prints "
    "the URL). For ordinary syncing use `jj git fetch` in your own workspace. "
    "Do not work around this gate."
)

UNRESOLVED_RECOVERY = (
    "Blocked: this command names cube's shared source store under %(root)s and "
    "runs `git %(sub)s`, a write, whose target the gate cannot resolve (it comes "
    "from command substitution or a variable the gate cannot see). The store is "
    "the single jj/git object store every leased workspace shares, so a write "
    "there that cannot be proven safe is refused. Spell the target directory out "
    "literally, keep it outside the store, and use jj inside your own workspace "
    "for VCS work; publish with the cube PR helpers "
    "(`\"$CUBE_BIN\" pr create` / `\"$CUBE_BIN\" pr update`); "
    "for CI-script reproduction use a throwaway clone under your "
    "own temp directory (`git clone <remote URL> \"$TMPDIR/ci-repro\"`). "
    "Read-only git against the store (log, show, diff, rev-parse, cat-file, "
    "ls-files, merge-base, ...) is allowed. Do not work around this gate."
)

SHELLS = frozenset(("sh", "bash", "zsh", "dash", "ksh"))
ASSIGNMENT_RE = re.compile(r"^([A-Za-z_][A-Za-z0-9_]*)=(.*)$", re.S)
VARIABLE_RE = re.compile(r"\$(?:\{([A-Za-z_][A-Za-z0-9_]*)\}|([A-Za-z_][A-Za-z0-9_]*))")

# Environment variables through which git locates a repository without
# being told on the command line. GIT_DIR wins over discovery; the others
# redirect pieces of the repository and count as targets in their own right.
GIT_LOCATION_ENV = ("GIT_DIR", "GIT_WORK_TREE", "GIT_COMMON_DIR", "GIT_OBJECT_DIRECTORY")

# Subcommands that never write to the repository they are pointed at.
# Anything not listed here (and not handled by a listing-form rule below)
# is treated as a write: default-deny.
READ_ONLY = frozenset((
    "log", "show", "diff", "diff-tree", "diff-index", "diff-files",
    "rev-parse", "rev-list", "cat-file", "ls-files", "ls-tree", "ls-remote",
    "merge-base", "status", "blame", "annotate", "name-rev", "describe",
    "show-ref", "for-each-ref", "shortlog", "whatchanged", "grep",
    "count-objects", "var", "version", "help", "check-ignore", "check-attr",
    "check-ref-format", "check-mailmap", "verify-pack", "verify-commit",
    "verify-tag", "show-branch", "range-diff", "cherry", "get-tar-commit-id",
    "patch-id", "stripspace", "column",
))

SHALLOW_SUBCOMMANDS = frozenset(("fetch", "pull", "clone"))
SHALLOW_FLAGS = (
    "--depth", "--shallow-since", "--shallow-exclude", "--update-shallow",
    "--deepen", "--unshallow",
)

# git global options that consume the following token.
GIT_GLOBAL_VALUE_OPTIONS = frozenset((
    "-C", "-c", "--git-dir", "--work-tree", "--namespace", "--super-prefix",
    "--config-env", "--attr-source", "--list-cmds",
))


class Unresolved(Exception):
    """A path that depends on shell state the gate cannot see."""


def expand(token, shell_vars):
    """Resolve $VAR / ${VAR} from the command's own assignments, then the
    hook environment. Anything still dynamic (an unknown variable, command
    substitution, a backtick) is Unresolved."""
    def substitute(match):
        name = match.group(1) or match.group(2)
        if name in shell_vars:
            value = shell_vars[name]
            if value is None:
                raise Unresolved(token)
            return value
        value = os.environ.get(name)
        if value is None:
            raise Unresolved(token)
        return value
    if "$(" in token or "`" in token:
        raise Unresolved(token)
    expanded = VARIABLE_RE.sub(substitute, token)
    if "$" in expanded:
        raise Unresolved(token)
    return os.path.expanduser(expanded)


def resolve_dir(token, cwd, shell_vars):
    """Absolute, normalised spelling of a path operand (no symlink following).

    A relative operand needs a known working directory; an absolute one
    resolves even when the gate lost track of the cwd."""
    value = expand(token, shell_vars)
    if not os.path.isabs(value):
        if cwd is None:
            raise Unresolved(token)
        value = os.path.join(cwd, value)
    return os.path.normpath(value)


def is_inside(child, parent):
    parent = parent.rstrip(os.sep)
    if not parent:
        return False
    return child == parent or child.startswith(parent + os.sep)


def inside_store(path, roots):
    """True when `path` -- as spelled or once symlinks are resolved -- lies
    under any spelling of the repos root."""
    forms = {path}
    try:
        forms.add(os.path.realpath(path))
    except Exception:
        pass
    return any(is_inside(form, root) for form in forms for root in roots)


def repos_root_for(start):
    """cube's repos root, derived from the nearest `.jj/repo` pointer file at
    or above `start`. None when `start` is not inside a cube secondary
    workspace."""
    try:
        current = os.path.abspath(start)
    except Exception:
        return None
    while True:
        pointer = os.path.join(current, ".jj", "repo")
        if os.path.isfile(pointer):
            try:
                with open(pointer, "r", encoding="utf-8") as handle:
                    raw = handle.read().strip()
            except Exception:
                return None
            target = raw if os.path.isabs(raw) else os.path.join(current, ".jj", raw)
            target = os.path.normpath(target)
            if os.path.basename(target) != "repo":
                return None
            jj_dir = os.path.dirname(target)
            if os.path.basename(jj_dir) != ".jj":
                return None
            return os.path.dirname(os.path.dirname(jj_dir))
        if os.path.isdir(pointer):
            # A primary jj workspace owns its store outright; it is not a
            # cube lease and there is no shared store behind it.
            return None
        parent = os.path.dirname(current)
        if parent == current:
            return None
        current = parent


def strip_prefixes(group, prefix_env, shell_vars):
    """Peel leading `K=V` assignments and launcher wrappers off a group,
    recording the assignments (they scope the environment of this command).
    Returns the remaining argv."""
    index = 0
    while index < len(group):
        token = group[index]
        if token in ("{", "}", "!", "if", "then", "else", "elif", "while", "until", "do", "time"):
            index += 1
            continue
        assignment = ASSIGNMENT_RE.match(token)
        if assignment:
            prefix_env[assignment.group(1)] = assignment.group(2)
            index += 1
            continue
        base = os.path.basename(token)
        if base == "env":
            index += 1
            while index < len(group) and group[index].startswith("-"):
                option = group[index]
                index += 1
                if option in ("-u", "--unset"):
                    index += 1
                elif option not in ("-i", "--ignore-environment", "--") and not option.startswith("--unset="):
                    raise Unresolved(option)
            continue
        if base in ("xargs", "parallel"):
            prefix_env["__APPENDS_ARGS"] = "1"
        if base in WRAPPERS or base in ("builtin", "parallel"):
            index += 1
            continue
        if base == "timeout" and index + 1 < len(group):
            index += 2
            continue
        break
    return [token for token in group[index:] if token != "}"]


def shallow_flag(args):
    for arg in args:
        for flag in SHALLOW_FLAGS:
            if arg == flag or arg.startswith(flag + "="):
                return flag
    return None


def has_flag(args, names):
    return any(arg in names or any(arg.startswith(name + "=") for name in names) for arg in args)


def positionals(args):
    return [arg for arg in args if not arg.startswith("-")]


def clone_positionals(args):
    values = {"--depth", "--branch", "-b", "--origin", "-o", "--upload-pack", "-u",
              "--template", "--reference", "--reference-if-able", "--separate-git-dir",
              "--config", "-c", "--shallow-since", "--shallow-exclude", "--jobs", "-j",
              "--filter", "--server-option", "--revision", "--bundle-uri", "--ref-format"}
    switches = {"--local", "-l", "--no-local", "--no-hardlinks", "--shared", "-s",
                "--quiet", "-q", "--verbose", "-v", "--progress", "--no-checkout", "-n",
                "--bare", "--mirror", "--single-branch", "--no-single-branch", "--no-tags",
                "--shallow-submodules", "--remote-submodules", "--sparse", "--dissociate",
                "--reject-shallow", "--no-reject-shallow", "--recurse-submodules",
                "--recursive", "--also-filter-submodules", "--ipv4", "-4", "--ipv6", "-6"}
    result = []
    index = 0
    while index < len(args):
        arg = args[index]
        if arg == "--":
            return result + args[index + 1:]
        if arg in values:
            index += 2
            continue
        if not arg.startswith("-"):
            result.append(arg)
        elif arg not in switches and arg.split("=", 1)[0] not in values:
            attached = any(arg.startswith(short) and len(arg) > len(short)
                           for short in values if short.startswith("-") and not short.startswith("--"))
            if not attached and not arg.startswith(("--recurse-submodules=", "--recursive=")):
                raise Unresolved(arg)
        index += 1
    return result


def is_read_only(sub, args, global_config):
    """Is `git <sub> <args>` clearly a read of the repository?"""
    if sub is None:
        return True  # `git --version`, `git --help`, bare `git`.
    if any(value.startswith("alias.") for value in global_config):
        return False  # `-c alias.show=fetch show`: the name lies.
    if sub in READ_ONLY:
        return not has_flag(args, ("--output", "--ext-diff", "--textconv"))
    pos = positionals(args)
    if sub == "branch":
        mutating = ("-d", "-D", "--delete", "-m", "-M", "--move", "-c", "-C", "--copy",
                    "-u", "--set-upstream-to", "--unset-upstream", "--edit-description",
                    "-f", "--force", "--track", "--no-track")
        if has_flag(args, mutating) or any(re.match(r"^-[^-]*[dDmMcCuf]", arg) for arg in args):
            return False
        return has_flag(args, ("--list", "-l", "--show-current", "--contains", "--no-contains", "--merged", "--no-merged", "--points-at")) or not pos
    if sub == "tag":
        mutating = ("-d", "--delete", "-a", "--annotate", "-s", "--sign", "-u", "--local-user",
                    "-f", "--force", "-m", "--message", "-F", "--file", "-e", "--edit")
        listing = ("-l", "--list", "-n", "--contains", "--no-contains", "--points-at",
                   "--merged", "--no-merged")
        if has_flag(args, mutating) or any(re.match(r"^-[^-]*[dasufmFe]", arg) for arg in args):
            return False
        if any(re.fullmatch(r"-n[0-9]*", arg) for arg in args):
            return True
        return has_flag(args, listing) or not pos
    if sub == "remote":
        return not pos or pos[0] in ("show", "get-url")
    if sub == "config":
        getters = ("--get", "--get-all", "--get-regexp", "--get-urlmatch", "--list", "-l",
                   "--get-color", "--get-colorbool")
        setters = ("--add", "--unset", "--unset-all", "--replace-all", "--rename-section",
                   "--remove-section", "--edit", "-e")
        return has_flag(args, getters) and not has_flag(args, setters)
    if sub == "stash":
        return bool(args) and args[0] in ("list", "show")
    if sub == "reflog":
        return not args or args[0] in ("show", "exists") or args[0].startswith("refs/")
    if sub == "worktree":
        return bool(args) and args[0] == "list"
    if sub == "notes":
        return bool(args) and args[0] in ("list", "show")
    if sub == "submodule":
        return not pos or pos[0] in ("status", "summary")
    if sub == "symbolic-ref":
        return len(pos) <= 1 and not has_flag(args, ("-d", "--delete"))
    if sub == "bundle":
        return bool(args) and args[0] in ("verify", "list-heads")
    if sub == "sparse-checkout":
        return bool(args) and args[0] == "list"
    if sub == "hash-object":
        return not any(arg.startswith("-") and not arg.startswith("--") and "w" in arg[1:] for arg in args)
    if sub == "apply":
        return has_flag(args, ("--check", "--stat", "--numstat", "--summary")) and not has_flag(args, ("--apply", "--reject", "--3way", "-3"))
    return False


def git_targets(sub, args, git_dir, location_env, cwd, c_dirs, shell_vars):
    """Every place this git invocation would read its repository from or
    write it to, as (label, resolved-or-None) pairs. None marks a target the
    gate could not resolve."""
    targets = []

    def add(label, token, base):
        try:
            targets.append((label, resolve_dir(token, base, shell_vars)))
        except Unresolved:
            targets.append((label, None))

    # `-C <dir>` moves the discovery root; later ones are relative to earlier.
    effective = cwd
    for directory in c_dirs:
        try:
            effective = resolve_dir(directory, effective, shell_vars)
        except Unresolved:
            effective = None
            break

    if git_dir is not None:
        add("--git-dir / GIT_DIR", git_dir, effective)
    elif sub == "clone":
        pos = clone_positionals(args)
        if len(pos) >= 2:
            add("clone destination", pos[1], effective)
        elif effective is None:
            targets.append(("clone destination", None))
        else:
            targets.append(("clone destination", effective))
    else:
        if effective is None:
            targets.append(("working directory", None))
        else:
            targets.append(("working directory", effective))

    for key in ("GIT_WORK_TREE", "GIT_COMMON_DIR", "GIT_OBJECT_DIRECTORY"):
        value = location_env.get(key)
        if value is not None:
            add(key, value, effective)

    # Subcommands whose positional operands are directories git creates or
    # populates: an explicit destination under the store is a store write.
    if sub in ("clone", "init", "worktree"):
        for index, arg in enumerate(args):
            if arg.startswith("--separate-git-dir="):
                add("--separate-git-dir", arg.split("=", 1)[1], effective)
            elif arg == "--separate-git-dir" and index + 1 < len(args):
                add("--separate-git-dir", args[index + 1], effective)
        if sub == "init":
            pos = positionals(args)
            if pos:
                add("init directory", pos[0], effective)
        if sub == "worktree":
            for operand in positionals(args)[1:]:
                add("worktree path", operand, effective)
    return targets


def parse_git(argv):
    """Split `git [globals] <sub> [args]` into its pieces."""
    git_dir = None
    work_tree = None
    c_dirs = []
    config = []
    sub = None
    sub_args = []
    index = 1
    while index < len(argv):
        token = argv[index]
        if token == "-C" and index + 1 < len(argv):
            c_dirs.append(argv[index + 1])
            index += 2
        elif token.startswith("-C") and len(token) > 2:
            c_dirs.append(token[2:])
            index += 1
        elif token.startswith("--git-dir="):
            git_dir = token[len("--git-dir="):]
            index += 1
        elif token == "--git-dir" and index + 1 < len(argv):
            git_dir = argv[index + 1]
            index += 2
        elif token.startswith("--work-tree="):
            work_tree = token[len("--work-tree="):]
            index += 1
        elif token == "--work-tree" and index + 1 < len(argv):
            work_tree = argv[index + 1]
            index += 2
        elif token == "-c" and index + 1 < len(argv):
            config.append(argv[index + 1])
            index += 2
        elif token in GIT_GLOBAL_VALUE_OPTIONS and index + 1 < len(argv):
            index += 2
        elif token.startswith("-"):
            if token not in ("--no-pager", "--paginate", "-p", "-P", "--bare", "--no-replace-objects", "--literal-pathspecs", "--no-optional-locks", "--version", "-v", "--help", "-h"):
                raise Unresolved(token)
            index += 1
        else:
            sub = token
            sub_args = argv[index + 1:]
            break
    return git_dir, work_tree, c_dirs, config, sub, sub_args


def emit(decision, reason=None):
    out = {"decision": decision}
    if reason is not None:
        out["reason"] = reason
    sys.stdout.write(json.dumps(out))
    sys.exit(0)


class Verdict(Exception):
    def __init__(self, reason):
        super().__init__(reason)
        self.reason = reason


def store_context(cwd, env, roots, needle_present):
    if needle_present or (cwd and inside_store(cwd, roots)):
        return True
    for key in GIT_LOCATION_ENV:
        value = env.get(key)
        if value:
            try:
                if inside_store(resolve_dir(value, cwd, env), roots):
                    return True
            except Unresolved:
                pass
    return False


def unparsed(detail, roots):
    return Verdict("Cannot prove shell operation safe near shared store %s: %s. Use jj/cube or a throwaway clone." % (roots[0], detail))


def plain_reader(argv):
    program = os.path.basename(argv[0])
    if program == "sed":
        # Only the plain line-printing form is a proven reader; sed's e/w
        # commands and script files can execute programs or write files.
        return len(argv) >= 3 and argv[1] == "-n" and bool(re.fullmatch(r"[0-9,$/ .^*+-]*p", argv[2])) and all(not a.startswith("-") for a in argv[3:])
    if program == "sort" and any(a.startswith("-o") for a in argv[1:]):
        return False
    if program == "uniq" and len(argv) > 2:
        return False
    if program == "awk":
        return len(argv) >= 2 and bool(re.fullmatch(r"\{\s*print(?:\s+\$[0-9]+)?\s*\}", argv[1])) and all(not a.startswith("-") and "=" not in a for a in argv[2:])
    return program in ("jj", "pwd", "ls", "cat", "head", "tail", "wc", "true", "false",
                       "grep", "rg", "sort", "uniq", "echo", "printf", "test", "[",
                       "fi", "done", "for", "in", "case", "esac", "do", "then", "else") and not any(
                           a in (">", ">>", "-o", "-f", "--files0-from") or a.startswith((">", "--output")) or a == "--pre" or a.startswith("--pre=") for a in argv[1:])


def judge_git(argv, prefix_env, exported, shell_vars, cwd, roots, needle_present):
    try:
        git_dir, work_tree, c_dirs, config, sub, sub_args = parse_git(argv)
    except Unresolved as error:
        if store_context(cwd, dict(exported, **prefix_env), roots, needle_present):
            raise unparsed("unrecognised git option " + str(error), roots)
        return
    location_env = {}
    for key in GIT_LOCATION_ENV:
        value = exported.get(key)
        if key in prefix_env:
            value = prefix_env[key]
        if value is not None:
            location_env[key] = value
    if git_dir is None:
        git_dir = location_env.get("GIT_DIR")
    if work_tree is not None:
        location_env["GIT_WORK_TREE"] = work_tree

    flag = shallow_flag(sub_args) if sub in SHALLOW_SUBCOMMANDS else None
    if flag is None and "__APPENDS_ARGS" not in prefix_env and is_read_only(sub, sub_args, config):
        return

    try:
        targets = git_targets(sub, sub_args, git_dir, location_env, cwd, c_dirs, shell_vars)
    except Unresolved as error:
        if store_context(cwd, dict(exported, **prefix_env), roots, needle_present):
            raise unparsed("unrecognised git option " + str(error), roots)
        return
    unresolved = None
    for label, resolved in targets:
        if resolved is None:
            unresolved = unresolved or label
            continue
        if inside_store(resolved, roots):
            detail = {"sub": sub, "root": roots[0], "target": resolved, "flag": flag}
            raise Verdict((SHALLOW_RECOVERY if flag else WRITE_RECOVERY) % detail)
    if unresolved is not None and (needle_present or flag):
        detail = {"sub": sub, "root": roots[0], "target": unresolved, "flag": flag}
        if flag:
            raise Verdict(SHALLOW_RECOVERY % detail)
        raise Verdict(UNRESOLVED_RECOVERY % detail)


def judge_command(command, cwd, exported, roots, needle_present, depth=0, written_paths=None):
    """Walk one shell command string, tracking cd / assignments / exports,
    and raise Verdict on the first git group that must be refused."""
    if depth > 4:
        raise Verdict(UNRESOLVED_RECOVERY % {"root": roots[0], "sub": "(nested shell)"})
    exported = dict(exported)
    shell_vars = dict(exported)
    allexport = False
    scopes = []
    uncertain_flow = False
    store_seen = False
    conditional_chain = False
    conditional_state = False
    pipeline_scope = None
    restore_pipeline = False
    written_paths = set(written_paths or ())
    if store_context(cwd, exported, roots, needle_present) and re.search(r'(?:\bfunction\s+\w+|\b\w+\s*\(\s*\))\s*\{', shell_text(command)):
        raise unparsed("function definition with unjudged shell state", roots)
    records = command_records(command)
    for record_index, group in enumerate(records):
        if restore_pipeline:
            cwd, exported, shell_vars, uncertain_flow, allexport = pipeline_scope
            exported, shell_vars = dict(exported), dict(shell_vars)
            restore_pipeline = False
            if group.boundary != "|":
                pipeline_scope = None
        if group and (pipeline_scope is not None or (record_index + 1 < len(records) and records[record_index + 1].boundary == "|")):
            if pipeline_scope is None:
                pipeline_scope = (cwd, dict(exported), dict(shell_vars), uncertain_flow, allexport)
            restore_pipeline = True
        involved = store_context(cwd, exported, roots, needle_present)
        store_seen = store_seen or involved
        if group.error:
            if involved:
                raise unparsed(group.error, roots)
            continue
        if group.boundary:
            if group.boundary == "&&":
                conditional_chain = True
            if group.boundary == ";":
                if conditional_state and store_seen:
                    uncertain_flow = True
                    cwd = None
                    needle_present = True
                conditional_chain = conditional_state = False
            if group.boundary in ("||", "&") and store_seen:
                uncertain_flow = True
                needle_present = True
                cwd = None
            for boundary in group.boundary:
                if boundary == "(":
                    scopes.append((cwd, dict(exported), dict(shell_vars), uncertain_flow, allexport))
                elif boundary == ")" and scopes:
                    cwd, exported, shell_vars, uncertain_flow, allexport = scopes.pop()
            continue
        if not group:
            continue
        for index, token in enumerate(group):
            redirect = re.fullmatch(r'(?:\d*|&)>+([^&].*)?', token)
            if redirect:
                target = redirect.group(1) or (group[index + 1] if index + 1 < len(group) else '')
                try:
                    written_paths.add(resolve_dir(target, cwd, shell_vars))
                except Unresolved:
                    pass
        if conditional_chain and (group[0] in ("cd", "pushd", "popd", "export", "unset", "set", "declare", "typeset", "local", "readonly") or any(ASSIGNMENT_RE.match(t) for t in group)):
            conditional_state = True
            if involved and (group[0] in ("export", "unset", "set", "declare", "typeset", "local", "readonly") or any(ASSIGNMENT_RE.match(t) for t in group)):
                raise unparsed("environment assignment in short-circuit control flow", roots)
        if group[0] in ("if", "elif", "else", "while", "until", "for", "case", "fi", "done", "esac"):
            # Branches and loops can run zero or many times. Do not adopt a
            # single branch's directory or exported environment as certain.
            uncertain_flow = True
        if uncertain_flow and involved:
            needle_present = True
            cwd = None
        if uncertain_flow and involved and (group[0] == "unset" or any(ASSIGNMENT_RE.match(t) for t in group)):
            raise unparsed("environment assignment in conditional control flow", roots)
        # Substitutions execute even inside assignments or read-only git arguments.
        if needle_present and any(re.search(r"(?:\$\(|`).*\bgit\b", token) for token in group):
            raise Verdict(UNRESOLVED_RECOVERY % {"root": roots[0], "sub": "(command substitution)"})
        prefix_env = {}
        try:
            argv = strip_prefixes(group, prefix_env, shell_vars)
        except Unresolved as error:
            if involved:
                raise unparsed("unrecognised wrapper option " + str(error), roots)
            continue
        if not argv:
            if restore_pipeline and involved:
                raise unparsed("assignment in a pipeline with shell-dependent scope", roots)
            if needle_present and any(re.search(r"(?<![\w./-])git(?![\w./-])", value) for value in prefix_env.values()):
                raise unparsed("git command stored in an assignment", roots)
            # Assignment-only group: `S=/path` persists for the rest of the command.
            for key, value in prefix_env.items():
                try:
                    shell_vars[key] = expand(value, shell_vars)
                except Unresolved:
                    shell_vars[key] = None
                if key in exported or allexport:
                    exported[key] = shell_vars[key]
            continue
        try:
            if "$" in argv[0] or "`" in argv[0]:
                argv = expand(argv[0], shell_vars).split() + argv[1:]
                argv = strip_prefixes(argv, prefix_env, shell_vars)
                if not argv:
                    raise Unresolved("empty command word")
        except Unresolved:
            if involved:
                raise unparsed("unresolved command word", roots)
            continue
        program = os.path.basename(argv[0])
        if restore_pipeline and involved and program in ("cd", "pushd", "popd", "export", "unset", "set", "declare", "typeset", "local", "readonly"):
            # Some shells run the final element in the parent. Neither its
            # mutations nor its lack of mutations can be assumed universally.
            raise unparsed("state-changing pipeline builtin", roots)
        if involved and "/" in argv[0]:
            try:
                executable = resolve_dir(argv[0], cwd, shell_vars)
            except Unresolved:
                executable = None
            if executable in written_paths:
                raise unparsed("execution of a path written by this command", roots)
        if program == "set":
            if "-a" in argv or argv[1:] == ["-o", "allexport"]:
                allexport = True
            if "+a" in argv or argv[1:] == ["+o", "allexport"]:
                allexport = False
            continue
        if program == "export" or (program in ("declare", "typeset", "local", "readonly") and any(t.startswith("-") and "x" in t for t in argv[1:])):
            for token in argv[1:]:
                if token.startswith("-"):
                    continue
                assignment = ASSIGNMENT_RE.match(token)
                if assignment:
                    try:
                        value = expand(assignment.group(2), shell_vars)
                    except Unresolved:
                        value = None
                    shell_vars[assignment.group(1)] = value
                    exported[assignment.group(1)] = value
                else:
                    exported[token] = shell_vars.get(token)
                if "-n" in argv:
                    exported.pop(token.split("=", 1)[0], None)
            continue
        if program == "unset":
            for token in argv[1:]:
                shell_vars.pop(token, None)
                exported.pop(token, None)
            continue
        if program in ("cd", "pushd"):
            argv = [argv[0]] + [a for a in argv[1:] if a not in ("-P", "-L", "--", "-n")]
            if len(argv) == 1:
                cwd = os.path.expanduser("~")
            elif argv[1] == "-":
                cwd = None
            else:
                try:
                    if (shell_vars.get("CDPATH") or prefix_env.get("CDPATH")) and not os.path.isabs(argv[1]):
                        raise Unresolved("CDPATH")
                    cwd = resolve_dir(argv[1], cwd, shell_vars)
                except Unresolved:
                    cwd = None
            continue
        if program == "popd":
            cwd = None
            continue
        if program in (".", "source"):
            if involved:
                raise unparsed("sourced script", roots)
            cwd = None
            exported = {key: None for key in exported}
            shell_vars = {key: None for key in shell_vars}
            continue
        if involved and "/" in argv[0] and program not in SHELLS and program not in ("git", "jj") and not plain_reader(argv):
            raise unparsed("unjudged executable path", roots)
        if needle_present and program not in ("git", "jj", "eval", "cat") and program not in SHELLS and any(re.search(r"\bgit\b", token) for token in argv[1:]):
            raise Verdict(UNRESOLVED_RECOVERY % {"root": roots[0], "sub": "(unparsed shell)"})
        if program != "git":
            for key in GIT_LOCATION_ENV:
                value = prefix_env.get(key, exported.get(key))
                if value is not None:
                    try:
                        target = resolve_dir(value, cwd, shell_vars)
                    except Unresolved:
                        target = None
                    if (target and inside_store(target, roots)) or (target is None and needle_present):
                        raise Verdict(WRITE_RECOVERY % {"sub": "(indirect via " + program + ")", "root": roots[0], "target": target})
            if ((cwd and inside_store(cwd, roots)) or (cwd is None and needle_present)) and not plain_reader(argv) and program not in SHELLS:
                raise unparsed("program " + program + " is not a proven reader with a store or unresolved working directory", roots)
        if program == "jj" and any(a in ("exec", "fix", "run") for a in argv[1:]):
            if "--" in argv:
                judge_command(shlex.join(argv[argv.index("--") + 1:]), cwd, dict(exported), roots, needle_present, depth + 1, written_paths)
            elif involved:
                raise unparsed("jj program launcher", roots)
            continue
        if re.fullmatch(r"(?:python[0-9.]*|perl|node|ruby)", program) and involved:
            raise unparsed("interpreter code via " + program, roots)
        if group.expanding_body and involved and re.search(r'\$\(|`', group.body):
            raise unparsed("command substitution in expanding heredoc", roots)
        if group.body and program != "cat" and involved:
            raise unparsed("heredoc code via " + program, roots)
        if program in SHELLS:
            # `sh -c '<script>'` (also `-lc`, `-ec`, ...): judge the script
            # in the same directory and environment.
            index = 1
            shell_body = None
            while index < len(argv):
                token = argv[index]
                if token == "--" or not token.startswith("-"):
                    break
                if token in ("-o", "-O", "--rcfile", "--init-file"):
                    index += 2
                    continue
                if token.startswith("-") and not token.startswith("--") and "c" in token[1:]:
                    merged = dict(exported)
                    merged.update({k: v for k, v in prefix_env.items() if k in GIT_LOCATION_ENV})
                    body_index = index + 1
                    if body_index < len(argv) and argv[body_index] == "--":
                        body_index += 1
                    if body_index >= len(argv):
                        if involved:
                            raise unparsed("missing shell code", roots)
                        break
                    shell_body = argv[body_index]
                    judge_command(shell_body, cwd, merged, roots, needle_present, depth + 1, written_paths)
                    break
                index += 1
            if shell_body is None:
                if involved:
                    raise Verdict(WRITE_RECOVERY % {"sub": "(script)", "root": roots[0], "target": cwd})
            continue
        if program == "eval":
            try:
                body = " ".join(expand(a, shell_vars) for a in argv[1:])
            except Unresolved:
                if involved:
                    raise unparsed("unresolved eval", roots)
                continue
            judge_command(body, cwd, dict(exported), roots, needle_present, depth + 1, written_paths)
            continue
        if program == "git":
            judge_git(argv, prefix_env, exported, shell_vars, cwd, roots, needle_present)
        elif program not in ("jj", "cat") and (needle_present or (cwd and inside_store(cwd, roots))) and any(re.search(r"\bgit\b", token) for token in argv):
            raise Verdict(UNRESOLVED_RECOVERY % {"root": roots[0], "sub": "(unparsed shell)"})


def main():
    try:
        payload = json.load(sys.stdin)
    except Exception as error:
        emit("block", MALFORMED + " Detail: hook stdin was not JSON (%s)." % error)
    if not isinstance(payload, dict):
        emit("block", MALFORMED + " Detail: hook payload was not a JSON object.")
    if (payload.get("tool_name") or "") != "Bash":
        emit("approve")
    tool_input = payload.get("tool_input")
    if not isinstance(tool_input, dict):
        emit("block", MALFORMED + " Detail: tool_input was not an object.")
    command = tool_input.get("command")
    if not isinstance(command, str):
        emit("block", MALFORMED + " Detail: tool_input.command was not a string.")
    masked = review_guide_masked_command(command)
    if masked is not None:
        command = masked

    cwd = payload.get("cwd")
    if not isinstance(cwd, str) or not cwd:
        cwd = os.getcwd()
    workspace = os.environ.get("BOSS_WORKSPACE", "").strip() or cwd
    repos_root = repos_root_for(workspace)
    if repos_root is None and workspace != cwd:
        repos_root = repos_root_for(cwd)
    if not repos_root:
        emit("approve")

    roots = [repos_root]
    try:
        real = os.path.realpath(repos_root)
        if real not in roots:
            roots.append(real)
    except Exception:
        pass
    needles = list(roots)
    home = os.path.expanduser("~")
    if repos_root.startswith(home + os.sep):
        needles.append(repos_root[len(home):].lstrip(os.sep))
    unescaped = command.replace("\\", "")
    needle_present = any(needle in command or needle in unescaped for needle in needles if needle)
    needle_present = needle_present or bool(re.search(r'\.jj/repo|\bjj\s+(?:workspace\s+)?root\b|(?:\.\./)+repos/', command))

    try:
        judge_command(command, cwd, dict(os.environ), roots, needle_present)
    except Verdict as verdict:
        emit("block", verdict.reason)
    emit("approve")


if __name__ == "__main__":
    main()
"##
    )
);

#[cfg(test)]
pub(crate) mod tests {
    //! Decision matrix for the shared-source-store write gate, run against the
    //! real script with a fake cube layout: `<tmp>/repos/flunge` is the shared
    //! store (a `.jj/repo` directory plus a `.git` directory, as cube lays it
    //! out) and `<tmp>/workspaces/ws` is a leased secondary workspace whose
    //! `.jj/repo` pointer names that store.

    use std::io::Write as _;
    use std::path::{Path, PathBuf};

    use tempfile::TempDir;

    use super::{GIT_STORE_GUARD_SCRIPT, GIT_STORE_GUARD_WORKSPACE_ENV};

    /// A fake cube layout. `store` is `<repos root>/flunge`, `workspace` the lease.
    pub(crate) struct CubeLayout {
        pub(crate) dir: TempDir,
        pub(crate) repos_root: PathBuf,
        pub(crate) store: PathBuf,
        pub(crate) workspace: PathBuf,
        pub(crate) script: PathBuf,
    }

    pub(crate) fn cube_layout() -> CubeLayout {
        let dir = TempDir::new().unwrap();
        let repos_root = dir.path().join("repos");
        let store = repos_root.join("flunge");
        std::fs::create_dir_all(store.join(".jj").join("repo")).unwrap();
        std::fs::create_dir_all(store.join(".git")).unwrap();
        std::fs::create_dir_all(store.join("cli")).unwrap();
        let workspace = dir.path().join("workspaces").join("ws");
        std::fs::create_dir_all(workspace.join(".jj")).unwrap();
        std::fs::create_dir_all(workspace.join("src")).unwrap();
        std::fs::write(
            workspace.join(".jj").join("repo"),
            store.join(".jj").join("repo").display().to_string(),
        )
        .unwrap();
        let script = dir.path().join("boss-git-store-guard.py");
        std::fs::write(&script, GIT_STORE_GUARD_SCRIPT).unwrap();
        CubeLayout {
            dir,
            repos_root,
            store,
            workspace,
            script,
        }
    }

    fn run_script(
        script: &Path,
        payload: &[u8],
        envs: &[(&str, &str)],
        unset: &[&str],
    ) -> (String, String, std::process::ExitStatus) {
        let mut cmd = std::process::Command::new("python3");
        cmd.arg(script);
        for key in [
            "GIT_DIR",
            "GIT_WORK_TREE",
            "GIT_COMMON_DIR",
            "GIT_OBJECT_DIRECTORY",
            GIT_STORE_GUARD_WORKSPACE_ENV,
        ] {
            cmd.env_remove(key);
        }
        for key in unset {
            cmd.env_remove(key);
        }
        for (key, value) in envs {
            cmd.env(key, value);
        }
        let mut child = cmd
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .expect("python3 must be available");
        child.stdin.as_mut().unwrap().write_all(payload).unwrap();
        drop(child.stdin.take());
        let out = child.wait_with_output().unwrap();
        (
            String::from_utf8_lossy(&out.stdout).into_owned(),
            String::from_utf8_lossy(&out.stderr).into_owned(),
            out.status,
        )
    }

    /// Decide `command` issued from `cwd` with `BOSS_WORKSPACE` naming the lease.
    pub(crate) fn decide_in(layout: &CubeLayout, command: &str, cwd: &Path, envs: &[(&str, &str)]) -> (String, String) {
        let payload = serde_json::json!({
            "tool_name": "Bash",
            "cwd": cwd.display().to_string(),
            "tool_input": {"command": command},
        });
        let workspace = layout.workspace.display().to_string();
        let mut all: Vec<(&str, &str)> = vec![(GIT_STORE_GUARD_WORKSPACE_ENV, workspace.as_str())];
        all.extend_from_slice(envs);
        let (stdout, stderr, status) = run_script(&layout.script, payload.to_string().as_bytes(), &all, &[]);
        assert!(status.success(), "guard must exit 0; stderr={stderr}");
        let value: serde_json::Value = serde_json::from_str(&stdout).unwrap_or_else(|e| panic!("{e}: {stdout}"));
        (
            value["decision"].as_str().unwrap().to_owned(),
            value["reason"].as_str().unwrap_or_default().to_owned(),
        )
    }

    fn decide(layout: &CubeLayout, command: &str) -> (String, String) {
        decide_in(layout, command, &layout.workspace, &[])
    }

    fn assert_blocked(layout: &CubeLayout, command: &str) -> String {
        let (decision, reason) = decide(layout, command);
        assert_eq!(decision, "block", "must block: {command}");
        reason
    }

    fn assert_approved(layout: &CubeLayout, command: &str) {
        let (decision, reason) = decide(layout, command);
        assert_eq!(decision, "approve", "must approve: {command} (reason: {reason})");
    }

    /// Shared regression corpus exercised through each driver's installed hook.
    pub(crate) fn approved_commands(layout: &CubeLayout) -> Vec<String> {
        let store = layout.store.display();
        vec![
            format!("git --git-dir {store}/.git log -1 2>&1 | head"),
            format!("git -C {store} log -1 && bazel build //x"),
            format!("cat <<'EOF'\n$(git -C {store} fetch --depth=200)\nEOF"),
        ]
    }

    /// Shared refusal corpus exercised through each driver's installed hook.
    pub(crate) fn bypass_commands(layout: &CubeLayout) -> Vec<String> {
        let store = layout.store.display();
        let script = layout.workspace.join("ci.sh");
        std::fs::write(
            &script,
            "#!/bin/sh
git fetch --depth=200 origin main
",
        )
        .unwrap();
        vec![
            format!("G=git; $G -C {store} fetch --depth=200 origin main"),
            format!("CMD='git -C {store} fetch --depth=1'; $CMD"),
            format!("CMD='git -C {store} fetch --depth=1'; eval \"$CMD\""),
            format!("STORE={store}; GIT=\"git -C $STORE\"; $GIT fetch --depth=1"),
            format!("cat > /tmp/x.sh <<'EOF'\ngit -C {store} fetch --depth=1\nEOF\n. /tmp/x.sh"),
            format!("cat > /tmp/x.sh <<'EOF'\ncd {store}\nEOF\nsource /tmp/x.sh; git fetch --depth=1"),
            format!("cat > /tmp/x.sh <<'EOF'\nexport GIT_DIR={store}/.git\nEOF\n. /tmp/x.sh; git fetch --depth=1"),
            format!("cat > /tmp/x.sh <<'EOF'\ngit -C {store} fetch --depth=1\nEOF\nchmod +x /tmp/x.sh; /tmp/x.sh"),
            format!("cat > /tmp/cat <<'EOF'\ngit -C {store} fetch --depth=1\nEOF\nchmod +x /tmp/cat; /tmp/cat"),
            format!("cd {store}; cd /tmp | cat; git fetch --depth=200 origin main"),
            format!("cd /tmp; echo x | cd {store}; git fetch --depth=200 origin main"),
            format!("export GIT_DIR={store}/.git; export GIT_DIR=/tmp/x/.git | cat; git fetch --depth=200"),
            format!("export GIT_DIR; GIT_DIR={store}/.git; git fetch --depth=200"),
            format!("set -a; GIT_DIR={store}/.git; git fetch --depth=200"),
            format!("set -o allexport; GIT_DIR={store}/.git; git fetch --depth=200"),
            format!("S={store}/.git; declare -x GIT_DIR=$S; git fetch --depth=200"),
            format!("S={store}/.git; typeset -x GIT_DIR=$S; git fetch --depth=200"),
            format!("S={store}/.git; local -x GIT_DIR=$S; git fetch --depth=200"),
            format!("S={store}/.git; readonly -x GIT_DIR=$S; git fetch --depth=200"),
            format!("cd {store}; bash {} -c true", script.display()),
            format!("cd {store}; bash -- {} -c true", script.display()),
            format!("cat <<EOF\n$(git -C {store} fetch --depth=200)\nEOF"),
            format!("cat <<EOF\n`git -C {store} fetch --depth=200`\nEOF"),
            format!("cd {store} # note; cd /tmp\ngit fetch --depth=200"),
            format!("cat # <<EOF\ngit -C {store} fetch --depth=200\nEOF"),
            format!("cd {store}; g() {{ cd /tmp; }}; git fetch --depth=200"),
            format!("cd {store}; function g {{ cd /tmp; }}; git fetch --depth=200"),
            "S=$(cat .jj/repo); cd \"$S/../..\" && git fetch --depth=200 origin main".to_owned(),
            "cd \"$(jj workspace root)/../../repos/flunge\" && git fetch --depth=200 origin main".to_owned(),
            "cd \"$UNKNOWN\"; git fetch --depth=200".to_owned(),
            format!("bash <<'EOF'\ncd {store} && git fetch --depth=200 origin main\nEOF"),
            format!("bash <<'EOF'\ngit -C {store} fetch --depth=200 origin main\nEOF"),
            format!("echo 'cd {store} && git fetch --depth=200 origin main' | sh"),
            format!("bash <<< 'git -C {store} fetch --depth=1'"),
            format!("bash -c -- 'git -C {store} fetch --depth=1'"),
            format!(r#"python3 -c "import subprocess; subprocess.run(['git','-C','{store}','fetch','--depth=1'])""#),
            format!("jj util exec -- git -C {store} fetch --depth=200 origin main"),
            format!("jj run -- git -C {store} fetch"),
            format!("jj fix -- git -C {store} fetch"),
            format!("cat > /tmp/x.sh <<'EOF'\ngit -C {store} fetch --depth=1\nEOF\nbash /tmp/x.sh"),
            format!("cd {store} && (cd /tmp) && git fetch --depth=200 origin main"),
            format!("cd {store}; if false; then cd /tmp; fi; git fetch --depth=200 origin main"),
            format!("cd {store}; if false; then cd /tmp; fi; bash ci.sh"),
            format!("cd {store}; false && cd /tmp; git fetch"),
            format!("cd {store}; false &&\n# continued conditional\ncd /tmp; git fetch"),
            format!("export GIT_DIR={store}/.git; false && export GIT_DIR=/tmp/x/.git; git fetch"),
            format!("cd {store}; cd /tmp & git fetch"),
            format!("export GIT_DIR=/tmp/scratch/.git; GIT_DIR={store}/.git; git fetch --depth=200 origin main"),
            format!(
                "export GIT_DIR=/tmp/scratch/.git; GIT_DIR={store}/.git; bash {}",
                script.display()
            ),
            format!("git \\\n-C {store} fetch --depth=200 origin main"),
            format!("GIT_DIR={store}/.git \\\ngit fetch --depth=200 origin main"),
            format!("echo fetch --depth=200 origin main | xargs git -C {store}"),
            format!("echo new-tag | xargs git -C {store} tag"),
            format!("echo new-branch | xargs git -C {store} branch"),
            format!("echo fetch | parallel git -C {store}"),
            format!("builtin cd {store} && git fetch --depth=200 origin main"),
            format!(
                "CDPATH={}; cd flunge && git fetch --depth=200 origin main",
                layout.repos_root.display()
            ),
            format!("cd {store} && sed -i s/a/b/ file"),
            format!(r#"cd {store} && awk 'BEGIN {{system("git fetch")}}'"#),
            format!("git -C {store} fetch 'unterminated"),
            format!("git -C {store} fetch --upload-pack='first\nsecond'"),
            format!("cd {store} && sort -ofile input"),
            format!("cd {store} && uniq input output"),
            format!("builtin pushd {store}; git fetch"),
            format!("(cd {store} && git fetch --depth=200 origin main)"),
            format!("(git -C {store} fetch --depth=200 origin main)"),
            format!("if true; then git -C {store} fetch --depth=1; fi"),
            format!("if false; then true; elif true; then git -C {store} push; else git -C {store} gc; fi"),
            format!("for r in x; do git -C {store} push; done"),
            format!("while true; do git -C {store} push; done"),
            format!("until false; do git -C {store} push; done"),
            format!("{{ cd {store}; git fetch --depth=1; }}"),
            format!("! git -C {store} fetch"),
            format!("time git -C {store} fetch"),
            format!("echo $(git -C {store} fetch)"),
            format!(r#"echo "$(git -C {store} fetch)""#),
            format!(r#"X="$(git -C {store} fetch)""#),
            format!(r#"export X="$(git -C {store} fetch)""#),
            format!(r#"git -C {store} log "$(git -C {store} fetch)""#),
            format!("bash ci.sh git -C {store} fetch"),
            format!("echo `git -C {store} fetch`"),
            format!("cd -P {store} && git fetch"),
            format!("pushd -L {store}; git gc"),
            format!("env -i GIT_DIR={store}/.git git fetch"),
            format!("env --ignore-environment --unset=OTHER GIT_DIR={store}/.git git fetch"),
            format!("GIT_DIR={store}/.git bash {}", script.display()),
            format!("export GIT_DIR={store}/.git && ./ci.sh"),
            format!("cd {store} && {}", script.display()),
            format!("cd {store} && bash {}", script.display()),
            format!("GIT_DIR={store}/.git make check"),
            format!("GIT_WORK_TREE={store} python3 script.py"),
            format!("GIT_COMMON_DIR={store}/.git make check"),
            format!("GIT_OBJECT_DIRECTORY={store}/.git/objects make check"),
            format!("git -C{store} fetch --depth=200"),
            format!("git -C{store}/cli -C.. fetch"),
            format!("git --git-dir-unknown={store}/.git fetch"),
            format!("git clone --depth 200 https://example.com/x.git {store}/new"),
            format!("git clone --branch main https://example.com/x.git {store}/new"),
            format!("git clone --branch=main -- https://example.com/x.git {store}/new"),
            format!("git -C {store} remote -v remove origin"),
            format!("git -C {store} remote --verbose set-url origin https://example.com/x.git"),
            format!("git -C {store} apply --stat --apply /tmp/x.patch"),
            format!("git -C {store} apply --check --reject /tmp/x.patch"),
            format!("git -C {store} config --show-origin core.bare true"),
            format!("git -C {store} reflog write refs/heads/main old new"),
            format!("git -C {store} interpret-trailers --in-place file"),
            format!("git -C {store} diff --output=file"),
            format!("git -C {store} branch -v new-branch"),
            format!("git -C {store} branch -ld old-branch"),
            format!("git clone --dep 200 https://example.com/x.git {store}/new"),
            format!("git -C {store} tag -fm message tag"),
            format!("git -C {store} hash-object -wt blob file"),
        ]
    }

    #[test]
    fn compound_syntax_scripts_and_option_bypasses_are_refused() {
        let layout = cube_layout();
        for command in bypass_commands(&layout) {
            assert_blocked(&layout, &command);
        }
        let store = layout.store.display();
        for command in [
            "git --no-advice status".to_owned(),
            "git --glob-pathspecs status".to_owned(),
            "env -C /tmp git --no-advice status".to_owned(),
            format!("cd {store} && git log | grep x"),
            format!("cd {store} && rg x"),
            format!("cd {store} && sed -n '1,5p'"),
            format!("cd {store} && awk '{{print $1}}'"),
            format!("cd {store} && echo hello"),
            format!("git --git-dir {store}/.git log -1 2>&1 | head"),
            format!("git --git-dir {store}/.git log -1 >&2 |& head"),
            format!("git --git-dir {store}/.git log -1 &>/tmp/log"),
            format!("git --git-dir {store}/.git log -1 &>>/tmp/log"),
            format!("git -C {store} log -1 && bazel build //x"),
            format!("export GIT_DIR; export -n GIT_DIR; GIT_DIR={store}/.git; git fetch"),
            format!("set -a; set +a; GIT_DIR={store}/.git; git fetch"),
            format!("export GIT_DIR; unset GIT_DIR; GIT_DIR={store}/.git; git fetch"),
            format!("cat <<'EOF'\n$(git -C {store} fetch)\nEOF"),
            format!("cat > /tmp/data <<'EOF'\ngit -C {store} fetch\nEOF"),
            format!("(cd {store}); git fetch --depth=1"),
        ] {
            assert_approved(&layout, &command);
        }

        for command in [
            format!("git -C{store} log -1"),
            format!("git -C{store}/cli -C.. status"),
            format!("(git -C {store} log -1)"),
            format!("if true; then git -C {store} log -1; fi"),
            format!("cd -P {store} && git log -1"),
            format!("env -i GIT_DIR={store}/.git git log -1"),
            format!("git -C {store} remote -v show origin"),
            format!("git -C {store} apply --stat /tmp/x.patch"),
            "git clone --depth 200 --branch main https://example.com/x.git /tmp/ci-repro".to_owned(),
        ] {
            assert_approved(&layout, &command);
        }
    }

    #[test]
    fn inherited_command_words_and_eval_are_expanded_before_dispatch() {
        let layout = cube_layout();
        let store = layout.store.display();
        let command = format!("git -C {store} fetch --depth=200");
        for (invocation, envs) in [
            (format!("$G -C {store} fetch --depth=200"), vec![("G", "git")]),
            ("$CMD".to_owned(), vec![("CMD", command.as_str())]),
            ("eval \"$CMD\"".to_owned(), vec![("CMD", command.as_str())]),
        ] {
            let (decision, reason) = decide_in(&layout, &invocation, &layout.workspace, &envs);
            assert_eq!(decision, "block", "{invocation}: {reason}");
        }
        let (decision, reason) = decide_in(
            &layout,
            &format!("$G -C {store} log -1"),
            &layout.workspace,
            &[("G", "git")],
        );
        assert_eq!(decision, "approve", "{reason}");
    }

    #[test]
    fn inherited_export_reassignment_uses_the_current_value() {
        let layout = cube_layout();
        for suffix in ["git fetch --depth=1", "bash ci.sh"] {
            let command = format!("GIT_DIR={}/.git; {suffix}", layout.store.display());
            let (decision, reason) = decide_in(
                &layout,
                &command,
                &layout.workspace,
                &[("GIT_DIR", "/tmp/scratch/.git")],
            );
            assert_eq!(decision, "block", "{command}: {reason}");
        }
        for command in [
            "if false; then unset GIT_DIR; fi; git fetch",
            "(unset GIT_DIR); git fetch",
        ] {
            let git_dir = layout.store.join(".git").display().to_string();
            let (decision, reason) = decide_in(&layout, command, &layout.workspace, &[("GIT_DIR", &git_dir)]);
            assert_eq!(decision, "block", "{command}: {reason}");
        }
        let (decision, reason) = decide_in(&layout, "if false; then cd /tmp; fi; git fetch", &layout.store, &[]);
        assert_eq!(decision, "block", "{reason}");
    }

    #[test]
    fn incident_shape_git_dir_env_prefix_with_shallow_fetch_is_refused() {
        let layout = cube_layout();
        let reason = assert_blocked(
            &layout,
            &format!(
                "GIT_DIR={} git fetch --depth=200 origin main",
                layout.store.join(".git").display()
            ),
        );
        assert!(reason.contains("shallow"), "{reason}");
        assert!(reason.contains("--depth"), "{reason}");
        assert!(reason.contains(&layout.repos_root.display().to_string()), "{reason}");
        assert!(reason.contains("jj git fetch"), "{reason}");
        assert!(reason.contains("throwaway clone"), "{reason}");
    }

    #[test]
    fn incident_shape_cd_into_store_then_shallow_fetch_is_refused() {
        let layout = cube_layout();
        let reason = assert_blocked(
            &layout,
            &format!("cd {} && git fetch --depth=200 origin main", layout.store.display()),
        );
        assert!(reason.contains("shallow"), "{reason}");
        // The separate-token spelling of --depth too.
        assert_blocked(
            &layout,
            &format!("cd {}; git fetch --depth 200 origin", layout.store.display()),
        );
    }

    #[test]
    fn shallow_options_are_refused_in_every_spelling_that_reaches_the_store() {
        let layout = cube_layout();
        let store = layout.store.display().to_string();
        for command in [
            format!("git -C {store} fetch --shallow-since=2026-07-01 origin"),
            format!("git -C {store} fetch --shallow-exclude v1.0 origin"),
            format!("git -C {store} pull --update-shallow"),
            format!("git -C {store} fetch --deepen=10"),
            format!("git -C {store} fetch --unshallow"),
            format!("git --git-dir={store}/.git fetch --depth=1 origin main"),
            format!("git --git-dir {store}/.git pull --depth=1"),
            format!("GIT_WORK_TREE={store} git --git-dir=/tmp/elsewhere/.git fetch --depth=1"),
            format!("git clone --depth=1 https://example.com/x.git {store}/x"),
            format!("cd {store}/cli && git fetch --depth=200"),
        ] {
            let reason = assert_blocked(&layout, &command);
            assert!(reason.contains("shallow"), "{command}: {reason}");
        }
    }

    #[test]
    fn exported_git_dir_earlier_in_the_command_counts() {
        let layout = cube_layout();
        let git = layout.store.join(".git").display().to_string();
        assert_blocked(
            &layout,
            &format!("export GIT_DIR={git}; git fetch --depth=200 origin main"),
        );
        assert_blocked(&layout, &format!("export GIT_DIR={git}\ngit pull"));
        assert_blocked(&layout, &format!("GIT_DIR={git}; export GIT_DIR; git gc"));
        // `unset` removes it again; the fetch then lands in the (non-store) workspace cwd.
        assert_approved(&layout, &format!("export GIT_DIR={git}; unset GIT_DIR; git fetch"));
    }

    #[test]
    fn inherited_git_dir_from_the_hook_environment_counts() {
        // The hook inherits the worker's environment. If something upstream still
        // exports GIT_DIR at the store, a bare `git fetch` from the workspace is a
        // store write and must be refused.
        let layout = cube_layout();
        let git = layout.store.join(".git").display().to_string();
        let (decision, reason) = decide_in(
            &layout,
            "git fetch --depth=200 origin main",
            &layout.workspace,
            &[("GIT_DIR", git.as_str())],
        );
        assert_eq!(decision, "block", "{reason}");
        let (decision, _) = decide_in(&layout, "git log -1", &layout.workspace, &[("GIT_DIR", git.as_str())]);
        assert_eq!(decision, "approve");
        for command in ["bash ci.sh", "make check", "python3 script.py"] {
            let (decision, reason) = decide_in(&layout, command, &layout.workspace, &[("GIT_DIR", git.as_str())]);
            assert_eq!(decision, "block", "{command}: {reason}");
            let (decision, reason) = decide_in(&layout, command, &layout.store, &[]);
            assert_eq!(decision, "block", "{command}: {reason}");
        }
    }

    #[test]
    fn non_shallow_writes_against_the_store_are_refused_with_redirection() {
        let layout = cube_layout();
        let store = layout.store.display().to_string();
        for command in [
            format!("git -C {store} fetch origin"),
            format!("git -C {store} pull"),
            format!("git -C {store} push origin main"),
            format!("git -C {store} checkout main"),
            format!("git -C {store} reset --hard HEAD~1"),
            format!("git -C {store} gc --prune=now"),
            format!("git -C {store} branch -D stale"),
            format!("git -C {store} branch new-branch"),
            format!("git -C {store} tag v9"),
            format!("git -C {store} tag -d v9"),
            format!("git -C {store} config core.sharedRepository true"),
            format!("git -C {store} remote set-url origin https://x"),
            format!("git -C {store} stash"),
            format!("git -C {store} reflog expire --all"),
            format!("git -C {store} worktree add /tmp/wt"),
            format!("git -C {store} update-ref refs/heads/main HEAD~5"),
            format!("git -C {store} repack -ad"),
            format!("git -C {store} some-alias"),
            format!("git -C {store} -c alias.log=fetch log"),
            format!("git -C /tmp/x worktree add {store}/wt"),
            format!("git init {store}/fresh"),
            format!("cd {store} && git status && git checkout -- ."),
            format!("GIT_DIR={store}/.git git commit --allow-empty -m x"),
            format!("GIT_COMMON_DIR={store}/.git git --git-dir=/tmp/x/.git fetch"),
            format!("git --git-dir={store}/.git --work-tree=/tmp/wt checkout -- ."),
            format!("git --work-tree={store} --git-dir=/tmp/wt/.git checkout -- ."),
        ] {
            let reason = assert_blocked(&layout, &command);
            assert!(!reason.contains("shallow option"), "{command}: {reason}");
            assert!(reason.contains("jj"), "{command}: {reason}");
            assert!(reason.contains("$CUBE_BIN"), "{command}: {reason}");
            assert!(reason.contains("throwaway clone"), "{command}: {reason}");
            assert!(reason.contains("Read-only git"), "{command}: {reason}");
        }
    }

    #[test]
    fn read_only_git_against_the_store_still_passes() {
        let layout = cube_layout();
        let store = layout.store.display().to_string();
        let git = format!("{store}/.git");
        for command in [
            format!("GIT_DIR={git} git log --oneline -5"),
            format!("GIT_DIR={git} git show HEAD:README.md"),
            format!("git --git-dir {git} diff --no-ext-diff --no-textconv abc def -- src/a.rs"),
            format!("git --git-dir={git} rev-parse HEAD"),
            format!("git -C {store} cat-file -t deadbeef"),
            format!("git -C {store} ls-files cli"),
            format!("git -C {store} merge-base main HEAD"),
            format!("git -C {store} status --short"),
            format!("git -C {store} blame README.md"),
            format!("git -C {store} branch -a"),
            format!("git -C {store} branch --show-current"),
            format!("git -C {store} tag -l 'v*'"),
            format!("git -C {store} config --get remote.origin.url"),
            format!("git -C {store} remote -v"),
            format!("git -C {store} stash list"),
            format!("git -C {store} reflog"),
            format!("git -C {store} worktree list"),
            format!("git -C {store} rev-list --count HEAD"),
            format!("git --no-pager -C {store} log -1"),
            format!("cd {store} && git log -3"),
            format!("cd {store}/cli && git diff HEAD~1"),
            format!("export GIT_DIR={git}; git describe --tags"),
            format!("git -C {store} --version"),
            format!("git -C {store} help fetch"),
            format!("git -C {store} grep -n needle"),
            format!("git -C {store} show-ref"),
            format!("git -C {store} for-each-ref refs/heads"),
            format!("git -C {store} hash-object README.md"),
            format!("git -C {store} apply --check /tmp/x.patch"),
        ] {
            assert_approved(&layout, &command);
        }
    }

    #[test]
    fn git_that_resolves_outside_the_store_is_not_this_gates_business() {
        let layout = cube_layout();
        let tmp = layout.dir.path().join("scratch");
        std::fs::create_dir_all(&tmp).unwrap();
        let tmp = tmp.display().to_string();
        for command in [
            // A throwaway clone under the worker's own temp dir is exactly what the
            // refusal tells the worker to do, shallow or not.
            format!("git clone --depth=1 https://example.com/flunge.git {tmp}/ci-repro"),
            format!("git clone {} {tmp}/ci-repro", layout.store.display()),
            format!("cd {tmp}/ci-repro && git fetch --depth=200 origin main"),
            format!("git -C {tmp}/ci-repro fetch --depth=200 origin main"),
            format!("GIT_DIR={tmp}/ci-repro/.git git fetch --depth=200"),
            format!("git -C {tmp}/ci-repro checkout -b x"),
            // A bare git from the workspace cwd targets the workspace, not the store.
            "git fetch --depth=200 origin main".to_owned(),
            "git status".to_owned(),
            "git push origin main".to_owned(),
            // jj is the sanctioned store writer; this gate does not judge it.
            "jj git fetch".to_owned(),
            "jj new main@origin".to_owned(),
            format!("cd {} && jj git fetch", layout.store.display()),
            // Not git at all.
            format!("ls {}", layout.store.display()),
            format!("rg -n depth {}/cli", layout.store.display()),
            format!("cat {}/.git/config", layout.store.display()),
            // The store path inside a quoted argument is data, not a target.
            format!(
                "jj describe -m 'reproduced with cd {} && git fetch --depth=200'",
                layout.store.display()
            ),
        ] {
            assert_approved(&layout, &command);
        }
    }

    #[test]
    fn single_level_shell_variables_are_resolved() {
        let layout = cube_layout();
        let store = layout.store.display().to_string();
        assert_blocked(&layout, &format!("S={store}; git -C \"$S\" fetch --depth=200"));
        assert_blocked(&layout, &format!("S={store}\ncd \"$S\" && git pull"));
        assert_blocked(&layout, &format!("D={store}/.git; GIT_DIR=$D git fetch origin"));
        assert_blocked(&layout, &format!("D={store}/.git; GIT_DIR=${{D}} git fetch origin"));
        assert_approved(&layout, &format!("S={store}; git -C \"$S\" log -1"));
    }

    #[test]
    fn nested_shells_and_eval_are_inspected() {
        let layout = cube_layout();
        let store = layout.store.display().to_string();
        assert_blocked(&layout, &format!("sh -c 'cd {store} && git fetch --depth=200'"));
        assert_blocked(&layout, &format!("bash -lc \"git -C {store} gc\""));
        assert_blocked(&layout, &format!("eval git -C {store} fetch"));
        assert_blocked(&layout, &format!("timeout 600 git -C {store} fetch"));
        assert_blocked(&layout, &format!("env GIT_DIR={store}/.git git fetch"));
        assert_blocked(&layout, &format!("nohup git -C {store} fetch"));
        assert_approved(&layout, &format!("sh -c 'cd {store} && git log -1'"));
    }

    #[test]
    fn unresolved_shallow_targets_fail_closed_even_without_a_store_literal() {
        let layout = cube_layout();
        let store = layout.store.display().to_string();
        // `cd` to something the gate cannot see, in a command that spells the
        // store out elsewhere: the write cannot be proven safe.
        let reason = assert_blocked(
            &layout,
            &format!("P=\"$(echo {store})\"; cd \"$P\" && git fetch --depth=200"),
        );
        assert!(reason.contains("shallow"), "{reason}");
        let reason = assert_blocked(&layout, &format!("cd \"$(dirname {store}/cli)\" && git gc"));
        assert!(reason.contains("cannot resolve"), "{reason}");
        // Shallow operations must resolve their destination inside a cube lease.
        assert_blocked(&layout, "cd \"$(mktemp -d)\" && git fetch --depth=200 origin");
        assert_approved(&layout, "P=\"$(mktemp -d)\"; git -C \"$P\" gc");
    }

    #[test]
    fn cwd_already_inside_the_store_is_a_store_target() {
        // The worker `cd`-ed into the store in an earlier tool call: the cwd the
        // harness reports on this call is the store itself.
        let layout = cube_layout();
        let (decision, reason) = decide_in(&layout, "git fetch --depth=200 origin main", &layout.store, &[]);
        assert_eq!(decision, "block", "{reason}");
        let (decision, _) = decide_in(&layout, "git fetch origin", &layout.store.join("cli"), &[]);
        assert_eq!(decision, "block");
        let (decision, _) = decide_in(&layout, "git log -1", &layout.store, &[]);
        assert_eq!(decision, "approve");
    }

    #[test]
    fn repos_root_is_derived_from_the_cwd_when_the_workspace_env_is_absent() {
        let layout = cube_layout();
        let payload = serde_json::json!({
            "tool_name": "Bash",
            "cwd": layout.workspace.join("src").display().to_string(),
            "tool_input": {"command": format!("cd {} && git fetch --depth=200", layout.store.display())},
        });
        let (stdout, stderr, status) = run_script(&layout.script, payload.to_string().as_bytes(), &[], &[]);
        assert!(status.success(), "{stderr}");
        assert!(stdout.contains("\"block\""), "{stdout}");
    }

    #[test]
    fn without_a_cube_pointer_there_is_no_store_to_protect() {
        let layout = cube_layout();
        let plain = layout.dir.path().join("plain-checkout");
        std::fs::create_dir_all(plain.join(".git")).unwrap();
        let payload = serde_json::json!({
            "tool_name": "Bash",
            "cwd": plain.display().to_string(),
            "tool_input": {"command": "git fetch --depth=200 origin main"},
        });
        let plain_str = plain.display().to_string();
        let (stdout, _, _) = run_script(
            &layout.script,
            payload.to_string().as_bytes(),
            &[(GIT_STORE_GUARD_WORKSPACE_ENV, plain_str.as_str())],
            &[],
        );
        assert!(stdout.contains("\"approve\""), "{stdout}");
    }

    #[test]
    fn non_shell_tools_are_approved_and_unreadable_payloads_block() {
        let layout = cube_layout();
        let workspace = layout.workspace.display().to_string();
        let env = [(GIT_STORE_GUARD_WORKSPACE_ENV, workspace.as_str())];
        for payload in [
            serde_json::json!({"tool_name": "Read", "tool_input": {"file_path": layout.store.join("x").display().to_string()}}),
            serde_json::json!({"tool_name": "Edit", "tool_input": {"file_path": "/tmp/x"}}),
            serde_json::json!({"tool_name": "apply_patch", "tool_input": {"command": "*** Begin Patch"}}),
        ] {
            let (stdout, _, _) = run_script(&layout.script, payload.to_string().as_bytes(), &env, &[]);
            assert!(stdout.contains("\"approve\""), "{payload}: {stdout}");
        }
        for payload in [
            b"not json".to_vec(),
            serde_json::json!(["a"]).to_string().into_bytes(),
            serde_json::json!({"tool_name": "Bash", "tool_input": "git fetch"})
                .to_string()
                .into_bytes(),
            serde_json::json!({"tool_name": "Bash", "tool_input": {"command": 7}})
                .to_string()
                .into_bytes(),
        ] {
            let (stdout, _, _) = run_script(&layout.script, &payload, &env, &[]);
            assert!(stdout.contains("\"block\""), "{stdout}");
            assert!(stdout.contains("fail-closed"), "{stdout}");
        }
    }

    #[test]
    fn review_guide_submission_body_is_data() {
        // A proven literal guide body may quote the incident command; the masker
        // keeps it from being lexed as shell.
        let layout = cube_layout();
        let command = format!(
            "\"$BOSS_BIN\" propose review-guide --body '# Guide\ncd {} && git fetch --depth=200\n'",
            layout.store.display()
        );
        assert_approved(&layout, &command);
    }

    #[test]
    fn messages_name_every_sanctioned_alternative() {
        let layout = cube_layout();
        let reason = assert_blocked(&layout, &format!("git -C {} fetch", layout.store.display()));
        for needle in [
            "jj git fetch",
            "pr create",
            "pr update",
            "throwaway clone",
            "$TMPDIR",
            "Read-only git",
            "--git-dir",
        ] {
            assert!(reason.contains(needle), "missing {needle:?} in: {reason}");
        }
        let reason = assert_blocked(&layout, &format!("git -C {} fetch --depth=1", layout.store.display()));
        for needle in ["shallow", "throwaway clone", "jj git fetch", "--depth"] {
            assert!(reason.contains(needle), "missing {needle:?} in: {reason}");
        }
    }
}
