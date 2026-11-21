//! Shared data-directory guard, used by every driver.

#[cfg(test)]
#[path = "path_guard_tests.rs"]
mod tests;

pub const PATH_GUARD_SCRIPT: &str = crate::render_review_guide_guard!(
    r#"#!/usr/bin/env python3
"""Deterministic Boss data-directory access gate (Claude Code PreToolUse hook).

Blocks any tool call whose target path canonically resolves inside the Boss
data directory (state.db, its -wal/-shm sidecars, the events socket, engine
pid/state files, and any future sidecar). Unlike an LLM classifier this does
not depend on the model recognising a path string in argv: it canonicalises
the working directory and every candidate path -- expanding ~, environment
variables and .. , and resolving symlinks -- then rejects on a component-wise
prefix match against the data directory.

The data directory is supplied via the BOSS_DATA_DIR environment variable,
set by the engine on the guard process. Worker shells never inherit that
value, so a Bash expansion of $BOSS_DATA_DIR is empty or attacker-controlled
while this process's expandvars would still resolve the real directory.
The PreToolUse payload arrives as JSON on stdin; a decision JSON is written
to stdout.

Tools this gate has nothing to say about are approved: there is no candidate
path to resolve, so silence is the right answer. The two tools it does read --
Bash, and Codex's apply_patch -- fail CLOSED: an unreadable payload for those
is blocked, because the guard then cannot tell whether the call targets the
data directory. A payload that is not JSON, or not a JSON object, fails closed
too: the gate cannot read the tool name from it, so it cannot claim to have
nothing to say.

A second, independent boundary blocks any tool call that would *start* a
recursive directory walk rooted at the whole machine -- /, /Users or a user
home, /Volumes or a mounted volume, /System, /Library, /private, /var, and the
/net and /home autofs maps. Only the root of a recursive walk is judged, so an
ordinary read of one specific file outside the workspace is untouched.
"""
import json
import os
import re
import shlex
import sys

"#,
    concat!(
        r#"

MALFORMED = (
    "Blocked (fail-closed): the data-directory gate could not read this "
    "tool call's payload, so it cannot tell whether the call targets the "
    "engine-owned data directory. Guards deny what they cannot parse rather "
    "than approving by default. Re-issue the operation as an ordinary shell "
    "command or file edit, and report this payload shape to the repo owner -- it "
    "means guard wiring needs updating for this agent driver."
)

# Codex `apply_patch` header lines that name a target path. The patch body
# arrives as `tool_input.command` (no `file_path` key), so these headers are
# the only place the touched paths appear.
PATCH_PATH_HEADERS = (
    "*** Add File:",
    "*** Update File:",
    "*** Delete File:",
    "*** Move to:",
)


def patch_target_paths(patch):
    """Every path named by an apply_patch header, in document order."""
    found = []
    for line in patch.splitlines():
        stripped = line.strip()
        for header in PATCH_PATH_HEADERS:
            if stripped.startswith(header):
                value = stripped[len(header):].strip()
                if value:
                    found.append(value)
    return found

RECOVERY = (
    "Blocked: direct access to the engine data directory "
    "(~/Library/Application Support/Boss) is not allowed from this "
    "session. That directory is engine-owned -- state.db, its "
    "-wal/-shm sidecars, the events socket, and engine pid/state files must "
    "never be read, copied, moved, edited, or opened by a session (no "
    "sqlite3, duckdb, litecli, sqlite-utils, cp/mv/rm, cat/head/tail/hexdump, "
    "editors, lsof, etc.). If progress needs a human to inspect or recover "
    "engine state, use `\"$BOSS_BIN\" propose blocked --reason \"<specific question>\"`. "
    "Do not work around this gate."
)

# -- Broad-traversal boundary ------------------------------------------------
#
# Roots whose subtree spans the operator's whole machine. A recursive walk
# rooted at one of these is never what a Boss worker needs: worker file access
# belongs in the leased workspace, the cube workspace/repo directories, and
# specific files a task actually names.
STATIC_BROAD_ROOTS = frozenset((
    "/",
    "/Users",
    "/Volumes",
    "/System",
    "/Library",
    "/private",
    "/var",
    # autofs maps -- descending these mounts a network volume on demand.
    "/net",
    "/home",
))

# Direct children of these are individual user homes / mounted volumes, also
# broader than any task root. Matched structurally so the guard never has to
# list (and therefore read) the directory itself.
BROAD_PARENTS = frozenset(("/Users", "/Volumes"))

# Programs that descend every directory handed to them. Their positional
# operands are parsed below; a program name alone is never a traversal root.
ALWAYS_RECURSIVE = frozenset((
    "find", "rg", "ripgrep", "ag", "ack", "fd", "fdfind",
    "tree", "du", "ncdu", "tar", "zip", "ditto",
))

# Programs that descend only when asked, keyed to the short flags that ask.
RECURSIVE_WITH_FLAG = {
    "grep": ("r", "R"),
    "ls": ("R",),
    "cp": ("r", "R"),
    "rsync": ("r", "a"),
}

# Long-form spellings of the same request.
RECURSIVE_LONG_FLAGS = frozenset((
    "--recursive",
    "--dereference-recursive",
    "--archive",
))

TRAVERSAL_RECOVERY = (
    "Blocked: this tool call starts a recursive directory walk rooted at %s, "
    "which spans the whole machine rather than the work. This "
    "session reads inside its leased workspace, the cube workspace/repo "
    "directories, and specific files a task names -- not the machine owner's home "
    "directory, other users' home directories, mounted volumes, or /. A walk "
    "that wide also reads private files belonging to other people on a shared "
    "Mac, and raises macOS privacy (TCC) prompts attributed to the app for "
    "Desktop, Documents, Downloads, Photos and network volumes. Re-run the "
    "search rooted at the directory you actually need -- the workspace root, "
    "or a named subdirectory under it. If you need one specific file outside "
    "the workspace, read that file directly instead of walking a tree to find "
    "it. Do not work around this gate."
)


# macOS firmlink. The data volume is mounted at /System/Volumes/Data and
# realpath() routinely resolves user-visible paths through it -- /home comes
# back as /System/Volumes/Data/home, /Users/x as /System/Volumes/Data/Users/x.
# The prefix is stripped before judging breadth so the same root is recognised
# in either spelling.
DATA_VOLUME_PREFIX = "/System/Volumes/Data"


def unfirmlinked(path):
    """`path` with the macOS data-volume firmlink prefix removed."""
    if path == DATA_VOLUME_PREFIX:
        return os.sep
    if path.startswith(DATA_VOLUME_PREFIX + os.sep):
        return path[len(DATA_VOLUME_PREFIX):]
    return path


def is_broad_root(path):
    """True when `path` roots a subtree that is out of scope for a worker."""
    for candidate in (path, unfirmlinked(path)):
        if candidate in STATIC_BROAD_ROOTS:
            return True
        parent, _, leaf = candidate.rpartition(os.sep)
        if leaf and parent in BROAD_PARENTS:
            return True
    try:
        if path == os.path.realpath(os.path.expanduser("~")):
            return True
    except Exception:
        pass
    return False


def asks_for_recursion(token, short_flags):
    """True when `token` is a flag asking this program to recurse."""
    if token in RECURSIVE_LONG_FLAGS:
        return True
    if not token.startswith("-") or token.startswith("--"):
        return False
    # Short flags cluster: `grep -rn` requests recursion just as `-r` does.
    return any(flag in token[1:] for flag in short_flags)


def literal_prefix(pattern):
    """The leading path of a glob pattern, before its first metacharacter."""
    cut = len(pattern)
    for meta in ("*", "?", "["):
        found = pattern.find(meta)
        if found != -1:
            cut = min(cut, found)
    return pattern[:cut]


SHELL_OPERATORS = frozenset((";", "&&", "||", "|", "&", "{", "}"))


def command_segments(tokens):
    """Shell command segments, with grouping and trailing operators stripped."""
    segment = []
    for token in tokens:
        token = token.rstrip(";&|").lstrip("(").rstrip(")")
        token = token.lstrip("{").rstrip("}")
        if not token or token in SHELL_OPERATORS:
            if segment:
                yield segment
                segment = []
            continue
        segment.append(token)
    if segment:
        yield segment


# Grep-family flags that *are* the pattern (`-e needle`, `-f patterns.txt`).
# After one of these, remaining positionals are traversal roots — there is
# no positional pattern left to skip.
GREP_PATTERN_FLAGS = frozenset((
    "-e", "--regexp",
    "-f", "--file",
))

# Grep-family flags whose next argument is a value, not a path or the pattern.
# Consumed so `rg -A 3 '/Users' tools` does not treat `3` as the pattern.
GREP_VALUE_FLAGS = frozenset((
    "-g", "--glob",
    "--iglob",
    "-t", "--type",
    "-A", "-B", "-C",
    "-m",
    "--max-depth",
))


def grep_roots(operands):
    """Traversal roots for grep-family commands after their regex pattern."""
    non_flags = []
    skip_next = False
    pattern_supplied = False
    for token in operands:
        if skip_next:
            skip_next = False
            continue
        if token in GREP_PATTERN_FLAGS:
            skip_next = True
            pattern_supplied = True
            continue
        if token in GREP_VALUE_FLAGS:
            skip_next = True
            continue
        if token.startswith("--regexp=") or token.startswith("--file="):
            pattern_supplied = True
            continue
        # Attached short forms: `-eneedle`, `-fpatterns.txt`.
        if (
            (token.startswith("-e") or token.startswith("-f"))
            and len(token) > 2
            and not token.startswith("--")
        ):
            pattern_supplied = True
            continue
        if token.startswith("-"):
            # `--flag=value` is one token and needs no skip.
            continue
        non_flags.append(token)
    if pattern_supplied:
        return non_flags
    return non_flags[1:]


def find_roots(operands):
    """`find` roots are its positional paths before its expression starts."""
    roots = []
    collecting = False
    for token in operands:
        # POSIX `-H`/`-L`/`-P` (and `--`) legally precede the path operands.
        if not collecting and token in ("-H", "-L", "-P", "--"):
            continue
        if token.startswith("-"):
            break
        collecting = True
        roots.append(token)
    return roots


def archive_roots(name, operands):
    """Traversal inputs for whole-tree archivers, excluding their outputs."""
    positional = [token for token in operands if not token.startswith("-")]
    if name == "ditto":
        return positional[:1]
    if name == "zip":
        # The first positional argument is the archive being written.
        return positional[1:]
    if name == "tar":
        # `-f archive` consumes its archive operand; the remaining positional
        # arguments are inputs. Handle both `-cf out.tar root` and `-f out`.
        roots = []
        skip_next = False
        for token in operands:
            if skip_next:
                skip_next = False
                continue
            if token == "-f" or (token.startswith("-") and "f" in token[1:]):
                skip_next = True
            elif not token.startswith("-"):
                roots.append(token)
        return roots
    return positional


def bash_traversal_roots(tokens):
    """Roots actually consumed by recursive Bash programs and prior `cd`s."""
    cd_roots = []
    roots = []
    for segment in command_segments(tokens):
        name = os.path.basename(segment[0])
        operands = segment[1:]
        if name == "cd" and operands:
            # A `cd` is relevant only when it precedes a recursive walk;
            # `cd /; echo ok` is not one.
            cd_roots.append(operands[0])
            continue
        if name == "mdfind":
            # mdfind searches the whole metadata index unless -onlyin scopes
            # it; its query string is not a path operand.
            roots.extend(cd_roots)
            for index, token in enumerate(operands[:-1]):
                if token == "-onlyin":
                    roots.append(operands[index + 1])
                    break
            else:
                roots.append(os.sep)
            continue
        if name == "locate":
            # locate has no directory-scoping flag: its database is global.
            roots.extend(cd_roots)
            roots.append(os.sep)
            continue
        recursive = name in ALWAYS_RECURSIVE
        short_flags = RECURSIVE_WITH_FLAG.get(name)
        if short_flags:
            recursive = any(asks_for_recursion(token, short_flags) for token in operands)
        if not recursive:
            continue
        roots.extend(cd_roots)
        if name in ("grep", "rg", "ripgrep", "ag", "ack"):
            roots.extend(grep_roots(operands))
        elif name == "find":
            roots.extend(find_roots(operands))
        elif name in ("tar", "zip", "ditto"):
            roots.extend(archive_roots(name, operands))
        else:
            roots.extend(token for token in operands if not token.startswith("-"))
    # Bash receives globs before the shell expands them in the hook payload.
    # Judge their literal prefix so `/Users/*/Desktop` cannot hide `/Users`.
    return [literal_prefix(root) or root for root in roots]


def traversal_roots(tool, tool_input, tokens):
    """Candidate roots for a recursive walk this tool call would start.

    Only the *root of a recursive walk* is judged -- an ordinary read of one
    specific file outside the workspace is none of this boundary's business.
    """
    roots = []
    if tool in ("Glob", "Grep", "Search"):
        path = tool_input.get("path")
        if isinstance(path, str) and path:
            roots.append(path)
        # Only Glob's pattern is a filesystem glob. Grep/Search patterns are
        # regular expressions, so their traversal root is exclusively `path`.
        for key in (("pattern", "glob") if tool == "Glob" else ()):
            value = tool_input.get(key)
            if isinstance(value, str) and value.startswith("/"):
                prefix = literal_prefix(value)
                if prefix:
                    roots.append(prefix)
    elif tokens:
        # Extract only operands that can establish a traversal root. This keeps
        # literal search patterns and flag values such as `-newer /Users` from
        # being mistaken for paths while still following `cd / && find .`.
        roots.extend(bash_traversal_roots(tokens))
    return roots


def expanded_path(path, cwd):
    """Absolute path with ~ and $VAR expanded, but symlinks left intact.

    The pre-realpath spelling matters for the traversal boundary: on macOS an
    autofs map such as /home realpath()s to a path that no longer looks like
    the root the caller named.
    """
    value = os.path.expanduser(os.path.expandvars(path))
    if not os.path.isabs(value):
        value = os.path.join(cwd, value)
    return os.path.normpath(value)


def canonical(path, cwd):
    return os.path.realpath(expanded_path(path, cwd))


def is_inside(child, parent):
    parent = parent.rstrip(os.sep)
    if not parent:
        return False
    return child == parent or child.startswith(parent + os.sep)


def emit(decision, reason=None):
    out = {"decision": decision}
    if reason is not None:
        out["reason"] = reason
    sys.stdout.write(json.dumps(out))
    sys.exit(0)


"#,
        crate::render_conversation_scratch!(),
        r#"
def conversation_scratch_path(candidate, cwd, payload):
    # The root comes from engine-owned hook wiring, never the payload.
    base = conversation_scratch_base(os.environ.get("BOSS_AGY_SCRATCH_ROOT"), payload.get("session_id"))
    return conversation_scratch_contains(canonical(candidate, cwd), base)


def main():
    raw_dir = os.environ.get("BOSS_DATA_DIR", "").strip()
    if not raw_dir:
        emit("approve")
    data_dir = os.path.realpath(os.path.expanduser(raw_dir))

    # A payload the gate cannot read at all is blocked, not approved. In that
    # state it cannot even determine the tool name, so the "a tool it has
    # nothing to say about" justification for approving does not apply: this is
    # an unanticipated payload shape, the exact condition the other Boss guards
    # refuse. (BOSS_DATA_DIR being unset, above, stays an approve -- that is
    # Boss's own configuration, not the agent's payload.)
    try:
        payload = json.load(sys.stdin)
    except Exception as error:
        emit("block", MALFORMED + " Detail: hook stdin was not JSON (%s)." % error)
    if not isinstance(payload, dict):
        emit("block", MALFORMED + " Detail: hook payload was not a JSON object.")

    tool = payload.get("tool_name") or ""
    tool_input = payload.get("tool_input")
    if not isinstance(tool_input, dict):
        tool_input = {}
    cwd = payload.get("cwd") or os.getcwd()

    candidates = []
    for key in ("file_path", "notebook_path", "path"):
        value = tool_input.get(key)
        if isinstance(value, str) and value:
            candidates.append(value)

    raw_command = ""
    tokens = []
    if tool == "Bash":
        command = tool_input.get("command")
        if not isinstance(command, str):
            emit("block", MALFORMED)
        masked = review_guide_masked_command(command)
        if masked is not None:
            command = masked
        raw_command = command
        try:
            lexer = shlex.shlex(command, posix=True, punctuation_chars=";&|")
            lexer.whitespace_split = True
            lexer.commenters = ""
            tokens = list(lexer)
        except Exception:
            tokens = command.split()
        candidates.extend(tokens)
        # Worker shells do not inherit BOSS_DATA_DIR. Any expansion of
        # $BOSS_DATA_DIR or ${BOSS_DATA_DIR} is therefore empty or
        # attacker-controlled, while expandvars still resolves this process's
        # value. Block every such expansion, including a plain assignment that
        # splits the path across the variable and a suffix.
        # Conservatively reject quoted/escaped literal mentions too: this raw
        # scan covers nested shell bodies whose outer quotes are literal.
        # Search for the bare name BOSS_DATA_DIR when editing guard sources.
        joined = raw_command.replace("\\\n", "")
        if re.search(r"\$(?:BOSS_DATA_DIR\b|\{BOSS_DATA_DIR\})", joined):
            emit("block", RECOVERY)
    elif tool == "apply_patch":
        # Codex's freeform apply_patch: the patch body is the "command".
        patch = tool_input.get("command")
        if patch is None:
            patch = tool_input.get("input")
        if not isinstance(patch, str):
            emit("block", MALFORMED)
        # The substring belt below applies to the patch text too: a path
        # written with ~ or a $VAR is not resolvable but is still readable.
        raw_command = patch
        candidates.extend(patch_target_paths(patch))

    scratch_candidates = []
    for candidate in candidates:
        try:
            if conversation_scratch_path(candidate, cwd, payload):
                scratch_candidates.append(candidate)
                continue
            if is_inside(canonical(candidate, cwd), data_dir):
                emit("block", RECOVERY)
        except Exception:
            continue

    # Second, independent boundary: refuse to *start* a recursive walk of the
    # machine. A traversal rooted at /, at a home directory, or at a volume
    # root reads the operator's private files (and, on a shared Mac, other
    # people's), and each one raises macOS privacy prompts attributed to Boss.
    # Evidence: tools/boss/docs/investigations/worker-filesystem-traversal-tcc-prompts-2026-08-19.md
    for root in traversal_roots(tool, tool_input, tokens):
        try:
            forms = (canonical(root, cwd), expanded_path(root, cwd))
        except Exception:
            continue
        for form in forms:
            if is_broad_root(form):
                emit("block", TRAVERSAL_RECOVERY % form)

    # Substring belt for Bash: catches $VAR / ~ indirection and backslash-
    # escaped spaces that tokenisation + canonicalisation miss (e.g.
    # P="$HOME/Library/Application Support/Boss/state.db"; sqlite3 "$P").
    # Needles are derived from the *non*-realpath expanded dir so the
    # home prefix matches the literal command text even when the real
    # home contains symlinks (realpath data_dir would diverge from the
    # $HOME the shell expands to).
    if raw_command:
        # Mask only complete validated path operands, retaining the belt for
        # every other occurrence (including assignments, sibling paths and
        # paths with traversal suffixes). Never skip the belt for a whole call.
        for candidate in sorted(scratch_candidates, key=len, reverse=True):
            spellings = ("'" + candidate + "'", '"' + candidate + '"',
                         candidate.replace(" ", "\\ "))
            for spelling in spellings:
                raw_command = re.sub(
                    r"(?<![^\s;&|<>])" + re.escape(spelling) + r"(?=$|[\s;&|<>])",
                    "AGY_SCRATCH", raw_command)
        expanded_dir = os.path.expanduser(raw_dir)
        needles = [data_dir, expanded_dir]
        home = os.path.expanduser("~")
        if expanded_dir.startswith(home + os.sep):
            needles.append(expanded_dir[len(home):].lstrip(os.sep))
        unescaped = raw_command.replace("\\", "")
        for needle in needles:
            if needle and (needle in raw_command or needle in unescaped):
                emit("block", RECOVERY)

    emit("approve")


if __name__ == "__main__":
    main()
"#
    )
);
