//! Shared Python fragments for driver `PreToolUse` guards.

/// Programs that cannot run in static-analysis-only worker sessions.
#[macro_export]
macro_rules! render_reviewer_blocked_programs {
    () => { "REVIEWER_BLOCKED={'make','just','cmake','ninja','meson','buck','xcodebuild','gradle','gradlew','mvn','mvnw','sbt','dotnet','pytest','tox','nox','rustfmt','gofmt','prettier','black','ruff','protoc','buf','codegen','npx','npm','pnpm','yarn','bun','deno','node','python','python3','ruby','perl','php','lua','java','kotlinc','swift','uv','poetry','pipenv','maturin','checkleft'}\n" };
}

pub const REVIEWER_BLOCKED_PROGRAMS_PY: &str = crate::render_reviewer_blocked_programs!();

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
    return word, bool(quote)

class CommandGroup(list):
    def __init__(self, tokens, body='', boundary=None, error=None):
        super().__init__(tokens)
        self.body = body
        self.expanding_body = False
        self.boundary = boundary
        self.error = error


def shell_text(text):
    # Comments cannot open quotes. Protect fd redirections from punctuation
    # splitting, while preserving quoted data exactly.
    out, quote, escaped, comment = [], None, False, False
    for index, char in enumerate(text):
        if comment:
            if char != chr(10):
                continue
            comment = False
        if escaped:
            # shlex preserves these escapes inside double quotes, while the
            # shell consumes them before executing an extracted -c body.
            if quote == chr(34) and char in (chr(36), chr(96)):
                out.pop()
            out.append(char)
            escaped = False
            continue
        if char == chr(92) and quote != chr(39):
            escaped = True
        elif quote:
            if char == quote:
                quote = None
        elif char in (chr(39), chr(34)):
            quote = char
        elif char == '#' and (index == 0 or text[index - 1].isspace() or text[index - 1] in ';|&()<>'):
            comment = True
            continue
        elif char == '&':
            before = text[index - 1:index]
            after = text[index + 1:index + 2]
            if before in ('>', '<') or after == '>':
                char = '__GUARD_FD_AMP__'
            elif before == '|':
                continue
        elif char == '|' and text[index - 1:index] == '>':
            char = '__GUARD_REDIRECT_PIPE__'
        out.append(char)
    return ''.join(out)


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
        pending = shell_text(pending + line + chr(10))
        try:
            lexer = shlex.shlex(pending, posix=True, punctuation_chars=';&|()')
            lexer.whitespace_split = True
            lexer.commenters = ''
            tokens = [token.replace('__GUARD_FD_AMP__', '&').replace('__GUARD_REDIRECT_PIPE__', '|') for token in lexer]
        except ValueError:
            # A quote can span physical lines. Never reinterpret failed lexing
            # as whitespace-separated argv.
            continue
        logical_line = pending
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
        delim = heredoc_delim(logical_line)
        if delim and groups:
            heredoc_end, quoted = delim
            receiver = groups[-1]
            receiver.expanding_body = not quoted
        if tokens and tokens[-1] not in ('&&', '||', '|'):
            groups.append(CommandGroup([], boundary=';'))
    if pending or heredoc_end:
        groups.append(CommandGroup([], error='incomplete shell syntax'))
    return groups


def command_groups(command):
    records = command_records(command)
    if any(group.error for group in records):
        # Keep every physical command visible to denylists after a lex error.
        return [part.split() for line in shell_text(command).splitlines()
                for part in re.split(r'[;&|()]', line) if part.split()]
    return [group for group in records if group]

def without_redirections(group):
    result = []
    index = 0
    while index < len(group):
        token = group[index]
        redirect = re.match(r'^(?:[0-9]+|&)?(?:>\||>>?|<>|<<-?|<)(.*)$', token)
        if redirect:
            if redirect.group(1) in ('', '&'):
                index += 1
        else:
            result.append(token)
        index += 1
    return result

def strip_prefixes(group):
    group = without_redirections(group)
    index = 0
    while index < len(group):
        token = group[index]
        base = os.path.basename(token)
        if ASSIGNMENT_RE.match(token):
            index += 1
            continue
        if base in WRAPPERS or base == 'timeout':
            index += 1
            value_options = {
                'env': {'-u', '--unset', '-C', '--chdir'},
                'sudo': {'-u', '--user', '-g', '--group', '-h', '--host', '-p', '--prompt'},
                'nice': {'-n', '--adjustment'},
                'ionice': {'-c', '--class', '-n', '--classdata', '-p', '--pid'},
                'stdbuf': {'-i', '-o', '-e', '--input', '--output', '--error'},
                'timeout': {'-k', '--kill-after', '-s', '--signal'},
                'xargs': {'-n', '-L', '-P', '-I', '-d', '--max-args', '--max-lines', '--max-procs', '--replace', '--delimiter'},
            }.get(base, set())
            while index < len(group) and group[index].startswith('-'):
                option = group[index]
                index += 1
                if base == 'command' and option in ('-v', '-V'):
                    return []
                if option == '--':
                    break
                if option in value_options and index < len(group):
                    index += 1
            if base == 'timeout' and index < len(group):
                index += 1
            continue
        break
    return group[index:]
"#, $after) };
}
pub const CODEX_COMMAND_TOKENIZER_PY: &str = crate::render_command_tokenizer!("", "");

/// Insert the shared tokenizer into a Python source template.
pub fn with_command_tokenizer(template: &str) -> String {
    template
        .replace("# COMMAND_TOKENIZER_FRAGMENT", CODEX_COMMAND_TOKENIZER_PY)
        .replace("# REVIEWER_BLOCKED_FRAGMENT", REVIEWER_BLOCKED_PROGRAMS_PY)
}

#[cfg(test)]
mod tests {
    #[test]
    fn command_queries_do_not_execute_the_queried_program() {
        let script = super::with_command_tokenizer(
            r#"
import os, re, shlex
# COMMAND_TOKENIZER_FRAGMENT
for command in ('command -v cube', 'command -V boss', 'command -v bossctl',
                'env command -v cube', 'command -p -v cube'):
    assert strip_prefixes(shlex.split(command)) == [], command
assert strip_prefixes(shlex.split('command cube')) == ['cube']
assert strip_prefixes(shlex.split('command -- bossctl')) == ['bossctl']
"#,
        );
        let output = std::process::Command::new("python3")
            .args(["-c", &script])
            .output()
            .unwrap();
        assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    }
}
