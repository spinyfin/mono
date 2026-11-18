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
WRAPPERS = {'env', 'command', 'builtin', 'exec', 'nohup', 'stdbuf', 'setsid', 'caffeinate', 'sudo', 'time', 'xargs', 'nice', 'ionice', 'chronic'}
ASSIGNMENT_RE = re.compile(r'^[A-Za-z_][A-Za-z0-9_]*=')

SHLEX_WS = ' \t\r\n'

def skip_arithmetic(line, index):
    # Double-paren and dollar-bracket arithmetic hold shifts, not redirections. Returns the index after the construct, or None if unclosed.
    if line.startswith(chr(36) + '[', index):
        depth, end = 1, index + 2
        opener, closer = '[', ']'
    else:
        depth, end = 2, index + 2
        opener, closer = '(', ')'
    while end < len(line) and depth:
        if line[end] == opener:
            depth += 1
        elif line[end] == closer:
            depth -= 1
        end += 1
    return None if depth else end


ANSI = 'ansi'


def is_ansi_open(text, index, quote):
    return quote is None and text.startswith(chr(36) + chr(39), index)


def heredoc_word(rest):
    # Read the whole delimiter word, applying bash quote removal. Any quote or
    # backslash anywhere in it makes the body non-expanding. False means the
    # syntax cannot be parsed safely.
    word, quoted, index = [], False, 0
    while index < len(rest):
        char = rest[index]
        if char in SHLEX_WS or char in ';&|()<>':
            break
        if char == chr(92):
            if index + 1 >= len(rest):
                return False
            word.append(rest[index + 1])
            quoted, index = True, index + 2
            continue
        if char == '$' and rest[index + 1:index + 2] in (chr(39), chr(34)):
            return False
        if char in (chr(39), chr(34)):
            quoted = True
            index += 1
            while index < len(rest) and rest[index] != char:
                if char == chr(34) and rest[index] == chr(92) and rest[index + 1:index + 2] in ('$', chr(96), chr(34), chr(92)):
                    index += 1
                word.append(rest[index])
                index += 1
            if index >= len(rest):
                return False
            index += 1
            continue
        word.append(char)
        index += 1
    return (''.join(word), quoted) if word else False


def heredoc_delim(line):
    # Unquoted << only. A quoted substring is an argument, not an operator.
    quote, escaped, index = None, False, 0
    while index < len(line):
        char = line[index]
        if escaped:
            escaped, index = False, index + 1
            continue
        if quote == ANSI:
            if char == chr(92):
                escaped, index = True, index + 1
                continue
            if char == chr(39):
                quote = None
            index += 1
            continue
        if quote != chr(39) and char == chr(92):
            escaped, index = True, index + 1
            continue
        if quote:
            if char == quote:
                quote = None
            index += 1
            continue
        if is_ansi_open(line, index, quote):
            quote = ANSI
            index += 2
            continue
        if char in (chr(39), chr(34)):
            quote = char
            index += 1
            continue
        if line.startswith(chr(36) + '[', index) or line.startswith('((', index):
            index = skip_arithmetic(line, index)
            if index is None:
                return False
            continue
        if char == '<' and index + 1 < len(line) and line[index + 1] == '<':
            rest = line[index + 2:]
            if rest.startswith('<'):
                return None
            if rest.startswith('-'):
                rest = rest[1:]
            rest = rest.lstrip(SHLEX_WS)
            return heredoc_word(rest)
        index += 1
    return None

class CommandGroup(list):
    def __init__(self, tokens, body='', boundary=None, error=None, literal_chars=None):
        super().__init__(tokens)
        self.body = body
        self.body_span = None
        self.literal_chars = literal_chars or [[] for _ in tokens]
        self.expanding_body = False
        self.boundary = boundary
        self.error = error


def split_punct_word(word, mask):
    if word and all(c in '();&|' for c in word):
        parts, masks, offset = [], [], 0
        for part in re.findall(r'[()]|[;&|]+', word):
            parts.append(part)
            masks.append(mask[offset:offset + len(part)])
            offset += len(part)
        return parts, masks
    return [word], [mask]


def quote_provenance(text, tokens):
    # Mirror shlex boundaries while retaining each literal character.
    words, masks, word, mask = [], [], [], []
    quote, index, active = None, 0, False
    def finish():
        nonlocal word, mask, active
        if active:
            parts, part_masks = split_punct_word(''.join(word), mask)
            words.extend(parts)
            masks.extend(part_masks)
        word, mask, active = [], [], False
    while index < len(text):
        char = text[index]
        if quote == ANSI:
            if char == chr(92) and index + 1 < len(text):
                word.append(text[index + 1])
                mask.append(True)
                active = True
                index += 2
                continue
            if char == chr(39):
                quote = None
                index += 1
                continue
            word.append(char)
            mask.append(True)
            active = True
            index += 1
            continue
        if quote != chr(39) and char == chr(92) and index + 1 < len(text):
            following = text[index + 1]
            if quote != chr(34) or following in (chr(34), chr(92)):
                word.append(following)
                mask.append(True)
                active = True
                index += 2
                continue
        if char == quote:
            quote = None
        elif is_ansi_open(text, index, quote):
            word.append(char)
            mask.append(False)
            quote = ANSI
            active = True
            index += 2
            continue
        elif quote is None and char in (chr(39), chr(34)):
            quote = char
            active = True
        elif quote is None and char in SHLEX_WS:
            finish()
        elif quote is None and char in ';&|()':
            finish()
            end = index + 1
            while end < len(text) and text[end] in ';&|()':
                end += 1
            for part in re.findall(r'[()]|[;&|]+', text[index:end]):
                words.append(part)
                masks.append([False] * len(part))
            index = end
            continue
        else:
            word.append(char)
            if quote == chr(39) or char == chr(2):
                mask.append(True)
            elif quote == chr(34):
                mask.append('d')
            else:
                mask.append(False)
            active = True
        index += 1
    finish()
    restored = []
    for position, word in enumerate(words):
        for marker, value in [('__GUARD_FD_AMP__', '&'), ('__GUARD_REDIRECT_PIPE__', '|'), (chr(2), '$')]:
            while marker in word:
                start = word.index(marker)
                masks[position][start:start + len(marker)] = [masks[position][start]]
                word = word[:start] + value + word[start + len(marker):]
        restored.append(word)
    if restored != tokens:
        return [[False] * len(token) for token in tokens]
    return masks


def open_quote(text):
    # The quote left open after lexing text, mirroring shlex posix quoting.
    quote, index = None, 0
    while index < len(text):
        char = text[index]
        if quote == ANSI:
            if char == chr(92):
                index += 2
                continue
            if char == chr(39):
                quote = None
            index += 1
            continue
        if quote != chr(39) and char == chr(92):
            index += 2
            continue
        if quote:
            if char == quote:
                quote = None
        elif is_ansi_open(text, index, quote):
            quote = ANSI
            index += 2
            continue
        elif char in (chr(39), chr(34)):
            quote = char
        index += 1
    return quote


def ansi_escapes_quote(text):
    # True when an unquoted $'...' string holds an escaped quote. Bash pairs
    # that quote inside the string while shlex closes the string there, so the
    # words shlex reports are not the words bash runs.
    quote, index = None, 0
    while index < len(text):
        char = text[index]
        if quote == ANSI:
            if char == chr(92):
                if text[index + 1:index + 2] == chr(39):
                    return True
                index += 2
                continue
            if char == chr(39):
                quote = None
            index += 1
            continue
        if quote != chr(39) and char == chr(92):
            index += 2
            continue
        if quote:
            if char == quote:
                quote = None
        elif is_ansi_open(text, index, quote):
            quote = ANSI
            index += 2
            continue
        elif char in (chr(39), chr(34)):
            quote = char
        index += 1
    return False


def odd_trailing_backslashes(text):
    return (len(text) - len(text.rstrip(chr(92)))) % 2 == 1


def strip_line_continuations(text):
    # Remove backslash-newline only where the shell does: not inside comments,
    # single quotes or non-expanding heredoc bodies.
    lines = text.split(chr(10))
    out, current, pending, carry = [], '', '', ''
    heredoc_end, expanding = None, False
    for number, line in enumerate(lines):
        last = number == len(lines) - 1
        if heredoc_end is not None:
            joined = carry + line
            if expanding and not last and odd_trailing_backslashes(joined):
                carry = joined[:-1]
                continue
            carry = ''
            out.append(joined)
            if joined.lstrip(chr(9)) == heredoc_end:
                heredoc_end = None
            continue
        shaped = shell_text(pending + line + chr(10))
        body = shaped[:-1]
        if not last and odd_trailing_backslashes(body) and open_quote(body[:-1]) != chr(39):
            pending = body[:-1]
            current += line[:-1]
            continue
        out.append(current + line)
        current = ''
        if open_quote(shaped):
            pending = shaped
            continue
        pending = ''
        delim = heredoc_delim(shaped)
        if delim:
            heredoc_end, expanding = delim[0], not delim[1]
    return chr(10).join(out + ([current] if current else []))


def shell_text(text):
    # Comments cannot open quotes. Protect fd redirections from punctuation
    # splitting, while preserving quoted data exactly.
    out, quote, escaped, comment, index = [], None, False, False, 0
    while index < len(text):
        char = text[index]
        if comment:
            if char == chr(10):
                comment = False
                out.append(char)
            index += 1
            continue
        if escaped:
            # shlex preserves these escapes inside double quotes, while the
            # shell consumes them before executing an extracted -c body.
            if quote == chr(34) and char in (chr(36), chr(96)):
                out.pop()
                if char == chr(36):
                    char = chr(2)
            out.append(char)
            escaped = False
            index += 1
            continue
        if char == chr(92) and quote != chr(39):
            escaped = True
        elif quote == ANSI:
            if char == chr(39):
                quote = None
        elif quote:
            if char == quote:
                quote = None
        elif is_ansi_open(text, index, quote):
            quote = ANSI
            out.append(char)
            out.append(text[index + 1])
            index += 2
            continue
        elif char in (chr(39), chr(34)):
            quote = char
        elif char == '#' and (index == 0 or text[index - 1].isspace() or text[index - 1] in ';|&()<>'):
            comment = True
            index += 1
            continue
        elif char == '&':
            before = text[index - 1:index]
            after = text[index + 1:index + 2]
            if before in ('>', '<') or after == '>':
                char = '__GUARD_FD_AMP__'
            elif before == '|':
                index += 1
                continue
        elif char == '|' and text[index - 1:index] == '>':
            char = '__GUARD_REDIRECT_PIPE__'
        out.append(char)
        index += 1
    return ''.join(out)


def command_records(command, stripped=False):
    groups = []
    pending = ''
    receiver = None
    heredoc_end = None
    if not stripped:
        command = strip_line_continuations(command)
    offset = 0
    for line in command.split(chr(10)):
        line_start = offset
        offset += len(line) + 1
        if heredoc_end is not None:
            if line.lstrip(chr(9)) == heredoc_end:
                heredoc_end = None
            else:
                receiver.body += line + chr(10)
                start = receiver.body_span[0] if receiver.body_span else line_start
                receiver.body_span = (start, min(offset, len(command)))
            continue
        pending = shell_text(pending + line + chr(10))
        if ansi_escapes_quote(pending):
            groups.append(CommandGroup([], error='incomplete shell syntax'))
            pending = ''
            continue
        try:
            lexer = shlex.shlex(pending, posix=True, punctuation_chars=';&|()')
            lexer.whitespace_split = True
            lexer.commenters = ''
            tokens = [token.replace('__GUARD_FD_AMP__', '&').replace('__GUARD_REDIRECT_PIPE__', '|').replace(chr(2), '$') for token in lexer]
        except ValueError:
            # A quote can span physical lines. If our tracker says every quote
            # is closed, shlex failed on a construct it does not model (ANSI-C
            # escaped quotes). Keep later lines visible instead of swallowing
            # them as continuation of an opener shlex invented.
            if open_quote(pending):
                continue
            groups.append(CommandGroup([], error='incomplete shell syntax'))
            pending = ''
            continue
        logical_line = pending
        pending = ''
        current = []
        tokens = [part for token in tokens for part in (re.findall(r'[()]|[;&|]+', token) if token and all(c in '();&|' for c in token) else [token])]
        try:
            literal = quote_provenance(logical_line, tokens)
        except Exception:
            literal = [[False] * len(token) for token in tokens]
        if len(literal) != len(tokens):
            literal = [[False] * len(token) for token in tokens]
        current_literal = []
        for index, token in enumerate(tokens):
            if token in DELIMS or (token and all(c in '()' for c in token)):
                if current:
                    groups.append(CommandGroup(current, literal_chars=current_literal))
                current = []
                current_literal = []
                groups.append(CommandGroup([], boundary=token))
            else:
                current.append(token)
                current_literal.append(literal[index])
        if current:
            groups.append(CommandGroup(current, literal_chars=current_literal))
        delim = heredoc_delim(logical_line)
        if delim is False:
            groups.append(CommandGroup([], error='incomplete shell syntax'))
        elif delim and groups:
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
for command, expected in (
    ('builtin eval', ['eval']),
    ('builtin source ./f', ['source', './f']),
    ('builtin cd /tmp', ['cd', '/tmp']),
    ('builtin command export NAME=x', ['export', 'NAME=x']),
    ('command builtin command export NAME=x', ['export', 'NAME=x']),
    ('builtin -- cd /tmp', ['cd', '/tmp']),
):
    assert strip_prefixes(shlex.split(command)) == expected, command
"#,
        );
        let output = std::process::Command::new("python3")
            .args(["-c", &script])
            .output()
            .unwrap();
        assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    }

    #[test]
    fn ansi_c_escaped_quote_yields_an_error_group() {
        let script = super::with_command_tokenizer(
            r#"
import os, re, shlex
# COMMAND_TOKENIZER_FRAGMENT
command = "echo $'\\'' ; cat /etc/passwd ; echo \\'"
assert any(g.error for g in command_records(command)), [list(g) for g in command_records(command)]
assert not any(g.error for g in command_records("echo $'a\\nb' x"))
"#,
        );
        let output = std::process::Command::new("python3")
            .args(["-c", &script])
            .output()
            .unwrap();
        assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    }

    #[test]
    fn ansi_c_quoting_does_not_open_a_heredoc() {
        let script = super::with_command_tokenizer(
            r#"
import os, re, shlex
# COMMAND_TOKENIZER_FRAGMENT
line = "echo $'\\' <<X #'"
assert heredoc_delim(shell_text(line)) is None, heredoc_delim(shell_text(line))
assert open_quote(line) is None
command = line + chr(10) + "cat /etc/passwd" + chr(10) + "X"
records = [list(g) for g in command_records(command) if list(g)]
assert any(g[:1] == ['cat'] for g in records), records
assert not any(getattr(g, 'expanding_body', False) for g in command_records(command))
"#,
        );
        let output = std::process::Command::new("python3")
            .args(["-c", &script])
            .output()
            .unwrap();
        assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    }
}
