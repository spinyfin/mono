//! Shared Python fragments for driver `PreToolUse` guards.

/// Programs that cannot run in static-analysis-only worker sessions.
#[macro_export]
macro_rules! render_reviewer_blocked_programs {
    () => { "REVIEWER_BLOCKED={'make','just','cmake','ninja','meson','buck','xcodebuild','gradle','gradlew','mvn','mvnw','sbt','dotnet','pytest','tox','nox','rustfmt','gofmt','prettier','black','ruff','protoc','buf','codegen','npx','npm','pnpm','yarn','bun','deno','node','python','python3','ruby','perl','php','lua','java','kotlinc','swift','uv','poetry','pipenv','maturin','checkleft'}\n" };
}

pub const REVIEWER_BLOCKED_PROGRAMS_PY: &str = crate::render_reviewer_blocked_programs!();

/// Conversation scratch policy shared by the adapter and data guard.
#[macro_export]
macro_rules! render_conversation_scratch {
    () => {
        r#"
def conversation_scratch_base(root, conversation):
    if not isinstance(root, str) or not root or not isinstance(conversation, str):
        return None
    if not re.fullmatch(r"[A-Za-z0-9][A-Za-z0-9_-]{0,127}", conversation):
        return None
    base = os.path.join(os.path.realpath(root), conversation)
    return base if os.path.realpath(base) == base else None


def conversation_scratch_contains(resolved, base):
    if base is None:
        return False
    # Keep ancestors and transcripts protected; never resolve allow roots.
    return any(resolved == allowed or resolved.startswith(allowed + os.sep)
               for allowed in (os.path.join(base, "scratch"),
                               os.path.join(base, ".system_generated", "steps"),
                               os.path.join(base, ".system_generated", "tasks")))


def glob_wildcard_uses_unmodeled_brackets(wildcard):
    # Python's glob/fnmatch does not implement POSIX classes, equivalence
    # classes, collating symbols or bash's `[^` negation. A `[` that appears
    # inside an already-open bracket expression in the same path component is
    # unmodelled, as are `[!` and `[^`.
    in_bracket = False
    for index, char in enumerate(wildcard):
        if char == os.sep:
            in_bracket = False
        elif char == '[':
            if in_bracket:
                return True
            nxt = wildcard[index + 1:index + 2]
            if nxt in ('!', '^'):
                return True
            in_bracket = True
        elif char == ']' and in_bracket:
            in_bracket = False
    return False
"#
    };
}

pub fn with_conversation_scratch(template: &str) -> String {
    template.replace("# CONVERSATION_SCRATCH_FRAGMENT", crate::render_conversation_scratch!())
}

/// Read-only-program classifier shared by the adapter and the data-dir guard.
///
/// Must be rendered after [`render_command_tokenizer!`]: `group_is_scratch_safe`
/// peels launcher prefixes with `strip_prefixes`.
#[macro_export]
macro_rules! render_scratch_safe_programs {
    () => {
        r#"
SCRATCH_SAFE_PROGRAMS = {
    "cat", "head", "tail", "wc", "ls", "echo", "printf", "true", "false", ":",
    "pwd", "test", "[", "dirname", "basename", "cd", "pushd", "popd",
}
SCRATCH_PATTERN_FILE_FLAGS = {
    "grep": ("-f", "--file"),
    "rg": ("-f", "--file"),
    "sed": ("-f", "--file"),
    "awk": ("-f", "--file"),
    "gawk": ("-f", "--file"),
}


def program_uses_pattern_file(rest):
    program = os.path.basename(rest[0]) if rest else ""
    flags = SCRATCH_PATTERN_FILE_FLAGS.get(program, ())
    if not flags:
        return False
    for arg in rest[1:]:
        if arg == "--":
            break
        if arg in flags:
            return True
        if any(arg.startswith(flag + "=") for flag in flags if flag.startswith("--")):
            return True
        if arg.startswith("-") and not arg.startswith("--"):
            for flag in flags:
                if len(flag) == 2 and flag[1] in arg[1:]:
                    return True
    return False


def sed_is_scratch_safe(args):
    has_n = False
    for arg in args:
        if arg == "--":
            break
        if arg in ("-i", "--in-place") or arg.startswith("--in-place="):
            return False
        if arg.startswith("-") and not arg.startswith("--") and "i" in arg[1:]:
            return False
        if arg in ("-n", "--quiet", "--silent") or (
                arg.startswith("-") and not arg.startswith("--") and "n" in arg[1:]):
            has_n = True
    return has_n


def group_is_scratch_safe(group):
    rest = strip_prefixes(group)
    if not rest:
        return True
    program = os.path.basename(rest[0])
    if program in SCRATCH_SAFE_PROGRAMS:
        return True
    if program in ("grep", "rg") and not program_uses_pattern_file(rest):
        return True
    if program == "sed" and sed_is_scratch_safe(rest[1:]):
        return True
    return False
"#
    };
}

pub const SCRATCH_SAFE_PROGRAMS_PY: &str = crate::render_scratch_safe_programs!();

/// Shell tokenizer shared by driver guards that inspect `Bash` commands.
///
/// This matches the lexer used by the shared Claude guards, including
/// heredoc metadata, logical-line grouping, shell boundaries, and
/// launcher-prefix removal.
#[macro_export]
macro_rules! render_command_tokenizer {
    ($before:expr, $after:expr) => { concat!($before, r#"
DELIMS = {'&&', '||', ';', '|', '&'}
WRAPPERS = {'env', 'command', 'builtin', 'exec', 'nohup', 'stdbuf', 'setsid', 'caffeinate', 'sudo', 'time', 'xargs', 'nice', 'ionice', 'chronic', 'timeout', 'doas', 'flock', 'chrt', 'taskset', 'script', 'watch', 'su', 'busybox', 'noglob', 'nocorrect', '-'}
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
    return (''.join(word), quoted, index) if word else False


def heredoc_ops(line):
    # Every unquoted, non-arithmetic << heredoc as (offset, word, quoted), or
    # False when the syntax cannot be parsed safely.
    found = []
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
                index += 3
                continue
            if rest.startswith('-'):
                rest = rest[1:]
            rest = rest.lstrip(SHLEX_WS)
            parsed = heredoc_word(rest)
            if parsed is False:
                return False
            found.append((index, parsed[0], parsed[1]))
            index += 2
            continue
        index += 1
    return found



class CommandGroup(list):
    def __init__(self, tokens, body='', boundary=None, error=None, literal_chars=None, quote_segments=None):
        super().__init__(tokens)
        self.body = body
        self.body_span = None
        self.literal_chars = literal_chars or [[] for _ in tokens]
        # Parallel to literal_chars: integer ids of the quote/escape run each
        # character came from. None means provenance did not match shlex, so
        # unbraced expansions cannot be verified.
        self.quote_segments = quote_segments
        self.expanding_body = False
        self.boundary = boundary
        self.error = error
        self.pipe_sources = []


def split_punct_word(word, mask, segs):
    if word and all(c in '();&|' for c in word):
        parts, masks, part_segs, offset = [], [], [], 0
        for part in re.findall(r'[()]|[;&|]+', word):
            parts.append(part)
            masks.append(mask[offset:offset + len(part)])
            part_segs.append(segs[offset:offset + len(part)])
            offset += len(part)
        return parts, masks, part_segs
    return [word], [mask], [segs]


def quote_provenance(text, tokens, spans=None):
    # Mirror shlex boundaries while retaining each literal character and the
    # quote/escape run it came from. Adjacent quoted strings concatenate into
    # one shlex word ("$A""B" -> $AB) but must not share a segment id.
    words, masks, segs, word, mask, segm = [], [], [], [], [], []
    quote, index, active, segment = None, 0, False, 0
    word_start = 0
    def new_segment():
        nonlocal segment
        segment += 1
    def finish():
        nonlocal word, mask, segm, active
        if active:
            parts, part_masks, part_segs = split_punct_word(''.join(word), mask, segm)
            if spans is not None:
                spans.extend([(word_start, index)] * len(parts))
            words.extend(parts)
            masks.extend(part_masks)
            segs.extend(part_segs)
        word, mask, segm, active = [], [], [], False
    while index < len(text):
        char = text[index]
        if not active and char not in SHLEX_WS:
            word_start = index
        if quote == ANSI:
            if char == chr(92) and index + 1 < len(text):
                word.append(text[index + 1])
                mask.append(True)
                segm.append(segment)
                active = True
                index += 2
                continue
            if char == chr(39):
                quote = None
                new_segment()
                index += 1
                continue
            word.append(char)
            mask.append(True)
            segm.append(segment)
            active = True
            index += 1
            continue
        if quote != chr(39) and char == chr(92) and index + 1 < len(text):
            following = text[index + 1]
            if quote != chr(34) or following in (chr(34), chr(92)):
                new_segment()
                word.append(following)
                mask.append(True)
                segm.append(segment)
                new_segment()
                active = True
                index += 2
                continue
        if char == quote:
            quote = None
            new_segment()
        elif is_ansi_open(text, index, quote):
            # The $ of $'...' is not an expansion. Keep it as its own run so
            # it cannot join the ANSI body into a trusted name.
            new_segment()
            word.append(char)
            mask.append(False)
            segm.append(segment)
            new_segment()
            quote = ANSI
            active = True
            index += 2
            continue
        elif quote is None and char in (chr(39), chr(34)):
            quote = char
            new_segment()
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
                if spans is not None:
                    spans.append((index, index + len(part)))
                    index += len(part)
                masks.append([False] * len(part))
                segs.append([segment] * len(part))
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
            segm.append(segment)
            active = True
        index += 1
    finish()
    restored = []
    for position, word in enumerate(words):
        for marker, value in [('__GUARD_FD_AMP__', '&'), ('__GUARD_REDIRECT_PIPE__', '|'), (chr(2), '$')]:
            while marker in word:
                start = word.index(marker)
                masks[position][start:start + len(marker)] = [masks[position][start]]
                segs[position][start:start + len(marker)] = [segs[position][start]]
                word = word[:start] + value + word[start + len(marker):]
        restored.append(word)
    if restored != tokens:
        return [[False] * len(token) for token in tokens], None
    return masks, segs


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
    heredocs = []
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
                if heredocs:
                    heredoc_end, quoted = heredocs.pop(0)
                    expanding = not quoted
                else:
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
        ops = heredoc_ops(shaped)
        if ops:
            heredocs = [op[1:] for op in ops]
            heredoc_end, quoted = heredocs.pop(0)
            expanding = not quoted
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
            # An escaped space is still inside a word, so a hash after that
            # space does not start a comment and later commands stay visible.
            slashes = 0
            j = index - 2
            while j >= 0 and text[j] == chr(92):
                slashes += 1
                j -= 1
            escaped_space = index > 0 and text[index - 1].isspace() and slashes % 2 == 1
            if not escaped_space:
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
    heredocs = []
    if not stripped:
        command = strip_line_continuations(command)
    offset = 0
    for line in command.split(chr(10)):
        line_start = offset
        offset += len(line) + 1
        if heredoc_end is not None:
            if line.lstrip(chr(9)) == heredoc_end:
                if heredocs:
                    receiver, heredoc_end, quoted = heredocs.pop(0)
                    receiver.expanding_body = not quoted
                else:
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
        line_group_start = len(groups)
        current = []
        tokens = [part for token in tokens for part in (re.findall(r'[()]|[;&|]+', token) if token and all(c in '();&|' for c in token) else [token])]
        spans = []
        try:
            literal, segments = quote_provenance(logical_line, tokens, spans)
        except Exception:
            literal = [[False] * len(token) for token in tokens]
            segments = None
        if literal is None or len(literal) != len(tokens):
            literal = [[False] * len(token) for token in tokens]
            segments = None
        elif segments is not None and len(segments) != len(tokens):
            segments = None
        if segments is None:
            spans = []
        current_literal = []
        current_segments = [] if segments is not None else None
        token_owners = {}
        for index, token in enumerate(tokens):
            if token in DELIMS or token in ('(', ')') or (token in ('{', '}') and not current and not any(literal[index])):
                if current:
                    groups.append(CommandGroup(current, literal_chars=current_literal, quote_segments=current_segments))
                current = []
                current_literal = []
                current_segments = [] if segments is not None else None
                groups.append(CommandGroup([], boundary=token))
            else:
                token_owners[index] = len(groups)
                current.append(token)
                current_literal.append(literal[index])
                if current_segments is not None:
                    current_segments.append(segments[index])
        if current:
            groups.append(CommandGroup(current, literal_chars=current_literal, quote_segments=current_segments))
        ops = heredoc_ops(logical_line)
        if ops is False:
            groups.append(CommandGroup([], error='incomplete shell syntax'))
        elif ops:
            owners = [g for g in groups[line_group_start:] if g] or [g for g in groups if g]
            if not owners:
                owners = [CommandGroup([])]
            for source_offset, word, quoted in ops:
                owner = None
                for i, (start, end) in enumerate(spans):
                    if start <= source_offset < end and i in token_owners:
                        owner = groups[token_owners[i]]
                        break
                if owner is None:
                    groups.append(CommandGroup([], error='cannot locate heredoc owner'))
                    owner = owners[-1]
                heredocs.append((owner, word, quoted))
            if heredocs:
                receiver, heredoc_end, quoted = heredocs.pop(0)
                receiver.expanding_body = not quoted
        if tokens and tokens[-1] not in ('&&', '||', '|'):
            groups.append(CommandGroup([], boundary=';'))
    if pending or heredoc_end:
        groups.append(CommandGroup([], error='incomplete shell syntax'))
    attach_pipe_sources(groups)
    return groups


def command_groups(command):
    records = command_records(command)
    if any(group.error for group in records):
        # Keep every physical command visible to denylists after a lex error.
        return [part.split() for line in shell_text(command).splitlines()
                for part in re.split(r'[;&|()]', line) if part.split()]
    return [group for group in records if group]

def without_redirection_indices(group):
    result = []
    index = 0
    while index < len(group):
        token = group[index]
        redirect = re.match(r'^(?:[0-9]+|&)?(?:>\||>>?|<>|<<-?|<)(.*)$', token)
        if redirect:
            attached = redirect.group(1)
            if attached in ('', '&'):
                index += 1
            elif attached.startswith('<'):
                if attached == '<':
                    index += 1
        else:
            result.append(index)
        index += 1
    return result

def without_redirections(group):
    return [group[i] for i in without_redirection_indices(group)]

WRAPPER_VALUE_OPTIONS = {
    'env': {'-u', '--unset', '-C', '--chdir', '-a', '--argv0'},
    'sudo': {'-u', '--user', '-g', '--group', '-h', '--host', '-p', '--prompt',
             '-C', '-T', '-R', '-D', '-r', '-t', '-U', '--chdir', '--chroot',
             '--role', '--type', '--other-user', '--close-from', '--command-timeout'},
    'exec': {'-a'},
    'nice': {'-n', '--adjustment'},
    'ionice': {'-c', '--class', '-n', '--classdata', '-p', '--pid', '-P', '-u', '--pgid', '--uid'},
    'stdbuf': {'-i', '-o', '-e', '--input', '--output', '--error'},
    'timeout': {'-k', '--kill-after', '-s', '--signal'},
    'xargs': {'-n', '-L', '-P', '-I', '-J', '-d', '-s', '-E', '--max-args', '--max-lines',
              '--max-procs', '--replace', '--delimiter'},
    'caffeinate': {'-t', '-w'},
    'doas': {'-u', '-C'},
    'flock': {'-w', '--timeout', '-E', '--conflict-exit-code', '-c', '--command'},
    'chrt': {'-T', '-P', '-D'},
    'taskset': {'-c', '--cpu-list'},
    'script': {'-t', '-F', '-c', '--command', '-I', '-O', '-B', '-T', '-E', '-m',
               '--log-in', '--log-out', '--log-io', '--log-timing', '--timing', '--output-limit'},
    'watch': {'-n', '--interval'},
    'su': {'-c', '--command', '-s', '--shell'},
}

WRAPPER_POSITIONAL = {'timeout': 1, 'flock': 1, 'chrt': 1, 'taskset': 1, 'script': 1}

def consume_wrapper(group, index, base):
    index += 1
    value_options = WRAPPER_VALUE_OPTIONS.get(base, set())
    positional = WRAPPER_POSITIONAL.get(base, 0)
    while index < len(group) and group[index].startswith('-'):
        option = group[index]
        index += 1
        if base == 'command' and option in ('-v', '-V'):
            return len(group)
        if option == '--':
            break
        if option in value_options and index < len(group):
            index += 1
        if base == 'taskset' and (option == '-c' or option.startswith('--cpu-list')):
            positional = 0
    return index + positional

def wrapper_basename(token):
    return os.path.basename(token[1:] if token.startswith('=') else token)

def strip_prefix_offset(group):
    index = 0
    while index < len(group):
        token = group[index]
        base = wrapper_basename(token)
        if ASSIGNMENT_RE.match(token):
            index += 1
        elif base in WRAPPERS:
            index = consume_wrapper(group, index, base)
        else:
            break
    return index

def expand_env_split_string(tokens):
    out, index = [], 0
    while index < len(tokens):
        token = tokens[index]
        out.append(token)
        index += 1
        if wrapper_basename(token) != 'env':
            continue
        while index < len(tokens) and tokens[index].startswith('-'):
            option = tokens[index]
            if option in ('-S', '--split-string') and index + 1 < len(tokens):
                out.extend(shlex.split(tokens[index + 1]))
                out.extend(tokens[index + 2:])
                return out
            if option.startswith('--split-string='):
                out.extend(shlex.split(option.split('=', 1)[1]))
                out.extend(tokens[index + 1:])
                return out
            if option.startswith('-S') and option != '-S':
                out.extend(shlex.split(option[2:]))
                out.extend(tokens[index + 1:])
                return out
            out.append(option)
            index += 1
            if option in WRAPPER_VALUE_OPTIONS.get('env', set()) and index < len(tokens):
                out.append(tokens[index])
                index += 1
            if option == '--':
                break
    return out

def strip_prefixes(group):
    stripped = expand_env_split_string(without_redirections(list(group)))
    return stripped[strip_prefix_offset(stripped):]

def wrapper_has_xargs(group):
    tokens = without_redirections(group)
    end = min(strip_prefix_offset(tokens), len(tokens))
    return any(wrapper_basename(tokens[i]) == 'xargs' for i in range(end))

def strip_prefix_indices(group):
    kept = without_redirection_indices(group)
    start = strip_prefix_offset([group[i] for i in kept])
    return kept[start:]

def write_redirection_targets(group):
    targets, index = [], 0
    while index < len(group):
        token = group[index]
        fd = 0
        while fd < len(token) and token[fd].isdigit():
            fd += 1
        rest = token[fd:]
        op = None
        for candidate in ('&>>', '&>', '>>', '>|', '>'):
            if rest.startswith(candidate):
                op = candidate
                break
        if op is None:
            index += 1
            continue
        attached = rest[len(op):]
        if attached:
            targets.append(attached)
        elif index + 1 < len(group):
            targets.append(group[index + 1])
            index += 1
        index += 1
    return targets

def here_string_body(group):
    for index, token in enumerate(group):
        if token == '<<<' and index + 1 < len(group):
            return group[index + 1]
        if token.startswith('<<<') and len(token) > 3:
            return token[3:]
    return ''

class _PipeLevel:
    def __init__(self):
        self.all_cmds = []
        self.pipeline = []
        self.pending_pipe = False
        self.inherited = []

def attach_pipe_sources(records):
    stack = [_PipeLevel()]
    for group in records:
        group.pipe_sources = []
        level = stack[-1]
        boundary = group.boundary
        if boundary in ('(', '{'):
            inner = _PipeLevel()
            if level.pending_pipe:
                inner.inherited = list(level.pipeline)
                level.pending_pipe = False
            stack.append(inner)
            continue
        if boundary in (')', '}'):
            inner = stack.pop()
            if not stack:
                stack.append(_PipeLevel())
            outer = stack[-1]
            producers = inner.inherited + inner.all_cmds
            outer.all_cmds.extend(producers)
            if outer.pending_pipe:
                outer.pipeline.extend(producers)
            else:
                outer.pipeline = list(producers)
            continue
        if boundary in ('|', '|&'):
            level.pending_pipe = True
            continue
        if boundary:
            if boundary.strip():
                level.pipeline = []
                level.pending_pipe = False
            continue
        if not group:
            continue
        sources = list(level.inherited)
        if level.pending_pipe:
            sources.extend(level.pipeline)
            level.pipeline.append(group)
            level.pending_pipe = False
        else:
            level.pipeline = [group]
        group.pipe_sources = sources
        level.all_cmds.append(group)

def decode_ansi_c(text):
    simple = {'n': chr(10), 't': chr(9), 'r': chr(13), 'a': chr(7), 'b': chr(8), 'e': chr(27),
              'E': chr(27), 'f': chr(12), 'v': chr(11), chr(92): chr(92), chr(39): chr(39),
              chr(34): chr(34), '?': '?'}
    widths = {'x': 2, 'u': 4, 'U': 8}
    out, i = [], 0
    while i < len(text):
        c = text[i]
        if c != chr(92) or i + 1 >= len(text):
            out.append(c)
            i += 1
            continue
        d = text[i + 1]
        if d in simple:
            out.append(simple[d])
            i += 2
        elif d in widths:
            digits = re.match('[0-9a-fA-F]*', text[i + 2:i + 2 + widths[d]]).group()
            if digits:
                out.append(chr(int(digits, 16)))
            else:
                out.append(c + d)
            i += 2 + len(digits)
        elif d in '01234567':
            digits = re.match('[0-7]{1,3}', text[i + 1:]).group()
            out.append(chr(int(digits, 8) % 256))
            i += 1 + len(digits)
        else:
            out.append(c + d)
            i += 2
    return ''.join(out)

def is_arithmetic_inner(text):
    if not text.startswith('('):
        return False
    try:
        return extract_substitutions(text, start=1, stop=')', kind='probe')['end'] == len(text) - 1
    except ValueError:
        return False

def extract_substitutions(source, holders=None, scripts=None, ariths=None, start=0, stop=None, kind='command'):
    # Replace dollar-paren, backtick, and process-substitution scripts with
    # globally numbered placeholders so nested checks share one holder registry.
    if holders is None:
        holders = []
    if scripts is None:
        scripts = set()
    if ariths is None:
        ariths = []
    if kind == 'command' and start == 0 and stop is None:
        source = strip_line_continuations(source)
    quotes, subs_on, command = kind != 'literal', kind != 'probe', kind == 'command'
    n = len(source)
    out, subs, pending = [], [], []
    quote, depth, found = None, 0, False
    word_start, arith_until, i = True, 0, start
    blanks = ' ' + chr(9) + chr(10) + chr(13) + ';&|('
    while i < n:
        c = source[i]
        if c == chr(92) and quote != chr(39):
            if source[i + 1:i + 2] == chr(10):
                i += 2
                continue
            out.append(source[i:i + 2])
            i += 2
            word_start = False
            continue
        if command and quote is None and word_start and c == '#':
            end = source.find(chr(10), i)
            i = n if end < 0 else end
            continue
        if stop == chr(96) and c == chr(96):
            found = True
            break
        if stop == ')' and quote is None and c in '()':
            if c == '(':
                depth += 1
            elif depth == 0:
                found = True
                break
            else:
                depth -= 1
        if quotes and c in (chr(39), chr(34)) and quote in (None, c):
            quote = None if quote else c
            out.append(c)
            i += 1
            word_start = False
            continue
        if quotes and quote is None and c == chr(36) and source[i + 1:i + 2] == chr(34):
            i += 1
            word_start = False
            continue
        if quotes and quote is None and c == chr(36) and source[i + 1:i + 2] == chr(39):
            end = i + 2
            while end < n and source[end] != chr(39):
                end += 2 if source[end] == chr(92) else 1
            if end >= n:
                raise ValueError('unterminated ANSI-C quote')
            decoded = decode_ansi_c(source[i + 2:end]).replace(chr(39), chr(39) + chr(92) + chr(39) + chr(39))
            out.append(chr(39) + decoded + chr(39))
            i = end + 1
            word_start = False
            continue
        if subs_on and quote != chr(39) and (c == chr(96) or source.startswith(chr(36) + '(', i)
                or (quote is None and source.startswith('<(', i))):
            if c == chr(96):
                inner = extract_substitutions(source, holders, scripts, ariths, i + 1, chr(96))
                j = inner['end']
                text = source[i + 1:j]
                subs.append(text)
            else:
                s, arithmetic = i + 2, False
                if source.startswith('((', i + 1):
                    inner = extract_substitutions(source, holders, scripts, ariths, s, ')', 'probe')
                    j = inner['end']
                    text = source[s:j]
                    arithmetic = is_arithmetic_inner(text)
                if arithmetic:
                    nested = extract_substitutions(text, holders, scripts, ariths, 0, None, 'arith')
                    subs.extend(nested['subs'])
                    ariths.append(text)
                else:
                    inner = extract_substitutions(source, holders, scripts, ariths, s, ')')
                    j = inner['end']
                    text = source[s:j]
                    subs.append(text)
            if source.startswith('<(', i):
                scripts.add(len(holders))
            holders.append(text)
            out.append('__indirect_pid_' + str(len(holders) - 1) + '__')
            i = j + 1
            word_start = False
            continue
        if command and quote is None and word_start and i >= arith_until and source.startswith('((', i):
            try:
                inner = extract_substitutions(source, holders, scripts, ariths, i + 2, ')', 'probe')
                j = inner['end']
                if source[j + 1:j + 2] == ')':
                    ariths.append(source[i + 2:j])
                    arith_until = j + 2
            except ValueError:
                pass
        if command and quote is None and c == '<' and i >= arith_until and source.startswith('<<', i):
            if source.startswith('<<<', i):
                out.append('<<<')
                i += 3
                word_start = False
                continue
            j = i + 2
            strip = j < n and source[j] == '-'
            j += 1 if strip else 0
            while j < n and source[j] in ' ' + chr(9):
                j += 1
            parsed = heredoc_word(source[j:])
            if not parsed:
                raise ValueError('unrecognised heredoc delimiter')
            delimiter, quoted, consumed = parsed
            end = j + consumed
            pending.append((delimiter, strip, quoted))
            out.append(source[i:end])
            i = end
            word_start = False
            continue
        if command and quote is None and c == chr(10):
            out.append(chr(10))
            i += 1
            for delimiter, strip, quoted in pending:
                body = []
                while True:
                    if i >= n:
                        raise ValueError('unterminated heredoc')
                    end = source.find(chr(10), i)
                    end = n if end < 0 else end
                    line = source[i:end]
                    i = end + 1
                    if (line.lstrip(chr(9)) if strip else line) == delimiter:
                        out.append(line)
                        if end < n:
                            out.append(chr(10))
                        break
                    if quoted:
                        out.append(line)
                    else:
                        nested = extract_substitutions(line, holders, scripts, ariths, 0, None, 'literal')
                        out.append(nested['out'])
                        subs.extend(nested['subs'])
                    out.append(chr(10))
            pending = []
            word_start = True
            continue
        out.append(c)
        i += 1
        if quote is None:
            word_start = c in blanks
    if stop is not None and not found:
        raise ValueError('unterminated command substitution')
    if pending:
        raise ValueError('unterminated heredoc')
    return {'out': ''.join(out), 'subs': subs, 'holders': holders, 'ariths': ariths,
            'scripts': scripts, 'end': i}
"#, $after) };
}
pub const CODEX_COMMAND_TOKENIZER_PY: &str = crate::render_command_tokenizer!("", "");

/// Insert the shared tokenizer into a Python source template.
pub fn with_command_tokenizer(template: &str) -> String {
    template
        .replace("# COMMAND_TOKENIZER_FRAGMENT", CODEX_COMMAND_TOKENIZER_PY)
        .replace("# REVIEWER_BLOCKED_FRAGMENT", REVIEWER_BLOCKED_PROGRAMS_PY)
        .replace("# SCRATCH_SAFE_PROGRAMS_FRAGMENT", SCRATCH_SAFE_PROGRAMS_PY)
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
assert heredoc_ops(shell_text(line)) == [], heredoc_ops(shell_text(line))
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

    #[test]
    fn quote_provenance_records_segment_boundaries() {
        let script = super::with_command_tokenizer(
            r#"
import os, re, shlex
# COMMAND_TOKENIZER_FRAGMENT
def token_segs(command, index=1):
    groups = [g for g in command_records(command) if list(g)]
    assert groups, command
    return groups[0][index], groups[0].quote_segments[index]
records = command_records('((1 << 3)); bash <<3\npkill -f foo\n3')
assert not any(g.error for g in records)
assert next(g for g in records if g and g[0] == 'bash').body == 'pkill -f foo\n'
assert all(not g.body for g in records if g and g[0] != 'bash')
command = "cat <<A; cat <<'B'\na\nA\nb" + chr(92) + "\nB\npkill -f foo"
assert strip_line_continuations(command) == command
records = command_records(command)
assert not any(g.error for g in records)
assert any(g and g[0] == 'pkill' for g in records)
word, segs = token_segs('echo "$A""B"')
assert word == '$AB', word
assert segs[0] == segs[1] != segs[2], segs
word, segs = token_segs('echo "$AB"')
assert word == '$AB', word
assert len(set(segs)) == 1, segs
word, segs = token_segs(r'echo $A\B')
assert word == '$AB', word
assert segs[0] == segs[1] != segs[2], segs
word, segs = token_segs("cat $'FOO'/x")
assert word.startswith('$FOO'), word
assert segs[0] != segs[1], segs
"#,
        );
        let output = std::process::Command::new("python3")
            .args(["-c", &script])
            .output()
            .unwrap();
        assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    }

    #[test]
    fn strip_prefix_indices_match_strip_prefixes() {
        let script = super::with_command_tokenizer(
            r#"
import os, re, shlex
# COMMAND_TOKENIZER_FRAGMENT
for command in (
    'echo x > y',
    'xargs -I {} echo \"$HOME\" {}',
    'FOO=bar env -u SECRET sudo -u nobody cat f > out',
    'command -v cube',
    'timeout 1 echo x 2>/dev/null',
):
    group = next(g for g in command_records(command) if list(g))
    assert group.quote_segments is not None, command
    assert len(group.literal_chars) == len(group), command
    stripped = strip_prefixes(group)
    kept = strip_prefix_indices(group)
    assert [group[i] for i in kept] == stripped, (command, kept, stripped)
    assert without_redirections(group) == [group[i] for i in without_redirection_indices(group)]
"#,
        );
        let output = std::process::Command::new("python3")
            .args(["-c", &script])
            .output()
            .unwrap();
        assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    }

    #[test]
    fn strip_prefixes_peels_flock_and_sudo_value_options() {
        let script = super::with_command_tokenizer(
            r#"
import os, re, shlex
# COMMAND_TOKENIZER_FRAGMENT
assert strip_prefixes(['flock', '/tmp/l', 'bossctl', 'status']) == ['bossctl', 'status']
assert strip_prefixes(['sudo', '-D', '/tmp', 'bossctl', 'status']) == ['bossctl', 'status']
assert strip_prefixes(['noglob', 'pkill', '-f', 'foo']) == ['pkill', '-f', 'foo']
assert strip_prefixes(['-', 'pkill', 'foo']) == ['pkill', 'foo']
assert wrapper_has_xargs(['pgrep', 'foo', '|', 'xargs', 'kill']) is False
assert wrapper_has_xargs(['xargs', 'kill']) is True
"#,
        );
        let output = std::process::Command::new("python3")
            .args(["-c", &script])
            .output()
            .unwrap();
        assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    }

    #[test]
    fn grouped_producers_contribute_every_command_to_pipe_sources() {
        let script = super::with_command_tokenizer(
            r#"
import os, re, shlex
# COMMAND_TOKENIZER_FRAGMENT
def consumer(command):
    records = [g for g in command_records(command) if list(g)]
    assert records, command
    return records[-1]
sh = consumer("(echo 'pkill -f foo'; echo done) | sh")
assert [list(g) for g in sh.pipe_sources] == [['echo', 'pkill -f foo'], ['echo', 'done']], [list(g) for g in sh.pipe_sources]
brace = consumer("{ echo 'pkill -f foo'; echo done; } | sh")
assert [list(g) for g in brace.pipe_sources] == [['echo', 'pkill -f foo'], ['echo', 'done']], [list(g) for g in brace.pipe_sources]
plain = consumer("echo a | echo b | sh")
assert [list(g) for g in plain.pipe_sources] == [['echo', 'a'], ['echo', 'b']], [list(g) for g in plain.pipe_sources]
"#,
        );
        let output = std::process::Command::new("python3")
            .args(["-c", &script])
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "stdout={} stderr={}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[test]
    fn extract_substitutions_numbers_holders_across_nested_scans() {
        let script = super::with_command_tokenizer(
            r#"
import os, re, shlex
# COMMAND_TOKENIZER_FRAGMENT
holders = []
outer = extract_substitutions('K=$(command -v pkill); bash -c "$(cat run.sh)"', holders)
assert holders == ['command -v pkill', 'cat run.sh'], holders
assert 'bash' in outer['out'] and '__indirect_pid_0__' in outer['out']
inner = extract_substitutions('echo $(date); __indirect_pid_0__ -f foo', holders)
assert holders[2] == 'date', holders
assert '__indirect_pid_2__' in inner['out']
assert '__indirect_pid_0__' in inner['out']
brew = extract_substitutions('eval "$(brew shellenv)"')
assert brew['holders'][0] == 'brew shellenv', brew
assert brew['out'].startswith('eval ')
"#,
        );
        let output = std::process::Command::new("python3")
            .args(["-c", &script])
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "stdout={} stderr={}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[test]
    fn glob_wildcard_detects_posix_class_later_in_bracket() {
        let script = super::with_conversation_scratch(
            r#"
import os, re
# CONVERSATION_SCRATCH_FRAGMENT
assert glob_wildcard_uses_unmodeled_brackets("[a[:lower:]]p")
assert glob_wildcard_uses_unmodeled_brackets("[x[=u=]]p")
assert glob_wildcard_uses_unmodeled_brackets("[x[.u.]]p")
assert glob_wildcard_uses_unmodeled_brackets("[[:lower:]]p")
assert glob_wildcard_uses_unmodeled_brackets("[!x]p")
assert glob_wildcard_uses_unmodeled_brackets("[^x]p")
assert not glob_wildcard_uses_unmodeled_brackets("[abc]p")
assert not glob_wildcard_uses_unmodeled_brackets("*.rs")
"#,
        );
        let output = std::process::Command::new("python3")
            .args(["-c", &script])
            .output()
            .unwrap();
        assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    }

    #[test]
    fn command_records_group_by_logical_line() {
        let script = super::with_command_tokenizer(
            r#"
import os, re, shlex
# COMMAND_TOKENIZER_FRAGMENT
records = [list(g) for g in command_groups("mv /tmp/p S/up\ncd S/up\nrg .")]
assert records[0][:1] == ["mv"], records
assert records[1][:1] == ["cd"], records
assert records[2][:1] == ["rg"], records
"#,
        );
        let output = std::process::Command::new("python3")
            .args(["-c", &script])
            .output()
            .unwrap();
        assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    }
}
