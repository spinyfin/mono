//! Shared Python fragments for Codex `PreToolUse` guards.

/// Shell tokenizer shared by Codex guards that inspect `Bash` commands.
///
/// This matches the lexer used by the shared Claude guards, including
/// heredoc handling, unspaced shell operators, per-line grouping, and
/// launcher-prefix removal.
pub const CODEX_COMMAND_TOKENIZER_PY: &str = r#"
DELIMS = {"&&", "||", ";", "|", "&"}
WRAPPERS = {"env", "command", "exec", "nohup", "stdbuf", "setsid", "caffeinate", "sudo", "time", "xargs"}
ASSIGNMENT_RE = re.compile(r"^[A-Za-z_][A-Za-z0-9_]*=")

def heredoc_delim(line):
    index = line.find("<<")
    if index < 0:
        return None
    rest = line[index + 2:]
    if rest.startswith("-"):
        rest = rest[1:]
    rest = rest.lstrip()
    if not rest:
        return None
    quote = rest[0] if rest[0] in (chr(39), chr(34)) else None
    if quote:
        rest = rest[1:]
    end = 0
    while end < len(rest) and (rest[end].isalnum() or rest[end] == "_"):
        end += 1
    word = rest[:end]
    if not word:
        return None
    if quote and (end == len(rest) or rest[end] != quote):
        return None
    return word

def command_groups(command):
    groups = []
    heredoc_end = None
    for line in command.split(chr(10)):
        if heredoc_end is not None:
            if line == heredoc_end:
                heredoc_end = None
            continue
        delim = heredoc_delim(line)
        if delim:
            heredoc_end = delim
        try:
            lexer = shlex.shlex(line, posix=True, punctuation_chars=";&|")
            lexer.whitespace_split = True
            lexer.commenters = ""
            tokens = list(lexer)
        except Exception:
            tokens = line.split()
        current = []
        for token in tokens:
            if token in DELIMS:
                if current:
                    groups.append(current)
                current = []
            else:
                current.append(token)
        if current:
            groups.append(current)
    return groups

def strip_prefixes(group):
    index = 0
    while index < len(group):
        token = group[index]
        base = os.path.basename(token)
        if ASSIGNMENT_RE.match(token) or base in WRAPPERS:
            index += 1
            continue
        if base == "timeout" and index + 1 < len(group):
            index += 2
            continue
        break
    return group[index:]
"#;

/// Insert the shared tokenizer into a Python source template.
pub fn with_command_tokenizer(template: &str) -> String {
    template.replace("# COMMAND_TOKENIZER_FRAGMENT", CODEX_COMMAND_TOKENIZER_PY)
}
