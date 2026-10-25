//! Shared Python fragments for driver `PreToolUse` guards.

/// Shell tokenizer shared by driver guards that inspect `Bash` commands.
///
/// This matches the lexer used by the shared Claude guards, including
/// heredoc metadata, logical-line grouping, shell boundaries, and
/// launcher-prefix removal.
#[macro_export]
macro_rules! render_command_tokenizer {
    ($before:expr, $after:expr) => { concat!($before, r#"
DELIMS = {'&&', '||', ';', '|', '&'}
WRAPPERS = {'env', 'command', 'exec', 'nohup', 'stdbuf', 'setsid', 'caffeinate', 'sudo', 'time', 'xargs', 'nice', 'ionice', 'chronic'}
ASSIGNMENT_RE = re.compile(r'^[A-Za-z_][A-Za-z0-9_]*=')

def heredoc_delim(line):
    index = line.find('<<')
    if index < 0:
        return None
    rest = line[index + 2:]
    if rest.startswith('-'):
        rest = rest[1:]
    rest = rest.lstrip()
    if not rest:
        return None
    quote = rest[0] if rest[0] in (chr(39), chr(34)) else None
    if quote:
        rest = rest[1:]
    end = 0
    while end < len(rest) and (rest[end].isalnum() or rest[end] == '_'):
        end += 1
    word = rest[:end]
    if not word:
        return None
    if quote and (end == len(rest) or rest[end] != quote):
        return None
    return word

class CommandGroup(list):
    def __init__(self, tokens, body='', boundary=None, error=None):
        super().__init__(tokens)
        self.body = body
        self.boundary = boundary
        self.error = error


def command_records(command):
    groups = []
    pending = ''
    receiver = None
    heredoc_end = None
    # Shell escaped newlines do not delimit commands.
    command = command.replace(chr(92) + chr(10), '')
    for line in command.split(chr(10)):
        if heredoc_end is not None:
            if line.lstrip(chr(9)) == heredoc_end:
                heredoc_end = None
            else:
                receiver.body += line + chr(10)
            continue
        pending += line + chr(10)
        try:
            lexer = shlex.shlex(pending, posix=True, punctuation_chars=';&|()')
            lexer.whitespace_split = True
            lexer.commenters = ''
            tokens = list(lexer)
        except ValueError:
            # A quote can span physical lines. Never reinterpret failed lexing
            # as whitespace-separated argv.
            continue
        pending = ''
        current = []
        tokens = [part for token in tokens for part in (re.findall(r'[()]|[;&|]+', token) if token and all(c in '();&|' for c in token) else [token])]
        for token in tokens:
            if token in DELIMS or (token and all(c in '()' for c in token)):
                if current:
                    groups.append(CommandGroup(current))
                current = []
                groups.append(CommandGroup([], boundary=token))
            else:
                current.append(token)
        if current:
            groups.append(CommandGroup(current))
        delim = heredoc_delim(line)
        if delim and groups:
            heredoc_end = delim
            receiver = groups[-1]
    if pending or heredoc_end:
        groups.append(CommandGroup([], error='incomplete shell syntax'))
    return groups


def command_groups(command):
    return [group for group in command_records(command) if group]

def strip_prefixes(group):
    index = 0
    while index < len(group):
        token = group[index]
        base = os.path.basename(token)
        if ASSIGNMENT_RE.match(token) or base in WRAPPERS:
            index += 1
            continue
        if base == 'timeout' and index + 1 < len(group):
            index += 2
            continue
        break
    return group[index:]
"#, $after) };
}
pub const CODEX_COMMAND_TOKENIZER_PY: &str = crate::render_command_tokenizer!("", "");

/// Insert the shared tokenizer into a Python source template.
pub fn with_command_tokenizer(template: &str) -> String {
    template.replace("# COMMAND_TOKENIZER_FRAGMENT", CODEX_COMMAND_TOKENIZER_PY)
}
