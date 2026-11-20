# Embedded in the always-on launch guard for every driver. Reuse its shell
# tokenizer; this fragment must contain no shell double quotes, backticks,
# dollars or backslashes because the enclosing command uses double quotes.
SIGNAL_REASON = ('Blocked unsafe process signalling: pkill/killall select processes by '
                 'name or pattern, and kill with indirect PIDs, process groups or broad '
                 'targets can terminate other workers. Stop only processes you started '
                 'yourself using explicit positive numeric PIDs (for example, the PID '
                 'captured from ' + chr(36) + '! when starting your process), or wait '
                 'for or time out your own command.')

SIG_NL, SIG_TAB, SIG_SQ, SIG_DQ = chr(10), chr(9), chr(39), chr(34)
SIG_BT, SIG_BS, SIG_DOLLAR = chr(96), chr(92), chr(36)
SIG_BANG = (SIG_DOLLAR + '!', SIG_DOLLAR + '{!}')
SIG_IDENT = re.compile('[A-Za-z_][A-Za-z0-9_]*')
SIG_ASSIGN = re.compile('^([A-Za-z_][A-Za-z0-9_]*)([+]?)=(.*)', re.S)
SIG_NAME = re.compile('(?<![A-Za-z_])(pkill|killall)(?![A-Za-z_])')
SIG_KILL_WORD = re.compile('(?<![A-Za-z_])(kill|pkill|killall)(?![A-Za-z_])')
SIG_HOLDER = re.compile('__indirect_pid_([0-9]+)__')
SIG_VARIABLE = re.compile(re.escape(SIG_DOLLAR) + '(?:[{]([A-Za-z_][A-Za-z0-9_]*)[}]|([A-Za-z_][A-Za-z0-9_]*))')
SIG_PARAM = re.compile(re.escape(SIG_DOLLAR) + '[{]([A-Za-z_][A-Za-z0-9_]*)(?![A-Za-z0-9_}])')
SIG_ARITH_SET = re.compile('([A-Za-z_][A-Za-z0-9_]*)[ ]*(?:[-+*/%&|^]|<<|>>)?=(?!=)')
SIG_ARITH_STEP = re.compile('([A-Za-z_][A-Za-z0-9_]*)[ ]*(?:[+][+]|--)|(?:[+][+]|--)[ ]*([A-Za-z_][A-Za-z0-9_]*)')
SIG_WORD_STEP = re.compile('([A-Za-z_][A-Za-z0-9_]*)(?:[+][+]|--)|[+][+]([A-Za-z_][A-Za-z0-9_]*)')
SIG_SHELLS = ('bash', 'sh', 'zsh', 'dash', 'ksh')
SIG_DECLARERS = ('export', 'declare', 'typeset', 'local', 'readonly')
SIG_BINDERS = ('read', 'mapfile', 'readarray', 'getopts', 'for', 'select', 'unset', 'wait')
SIG_EVALUATORS = ('let', 'eval', 'source', '.')
SIG_KEYWORDS = ('!', '{', '}', 'then', 'do', 'if', 'elif', 'while', 'until', 'else')

def signal_shell_operands(words):
    i = 0
    while i < len(words):
        word = words[i]
        if word == '--':
            return words[i+1:]
        if word in ('-o', '+o', '-O', '+O', '--rcfile', '--init-file'):
            i += 2
        elif word[:1] in ('-', '+'):
            i += 1
        else:
            return words[i:]
    return []

def signal_operand(words):
    return signal_shell_operands(words)[:1]

def signal_launcher_strings(words):
    if not words:
        return []
    _, base = signal_program(words[0])
    if base in SIG_SHELLS or base in ('su', 'flock', 'script'):
        return signal_cstrings(words[1:])
    if base == 'watch':
        extra = words[1:]
        while extra and extra[0].startswith('-'):
            extra = extra[2:] if extra[0] in ('-n', '--interval') else extra[1:]
        return [' '.join(extra)]
    return []

def signal_cstrings(words):
    # Shell command strings handed to a launcher with -c or --command.
    found = []
    for i, word in enumerate(words):
        if word.startswith('--command='):
            found.append(word.split('=', 1)[1])
        elif i + 1 < len(words) and (word == '--command' or (
                word.startswith('-') and not word.startswith('--') and 'c' in word)):
            found.extend(signal_operand(words[i+1:]))
    return found

def signal_program(word):
    if word.startswith('='):
        word = word[1:]
    return word, os.path.basename(word)

def signal_split_heredocs(group):
    # A heredoc attached to its command (cat<<EOF) is one shell word; split the
    # unquoted operator off so the program and its redirections read normally.
    masks = getattr(group, 'literal_chars', None) or []
    words = []
    for i, word in enumerate(group):
        mask = masks[i] if i < len(masks) else [True] * len(word)
        at = word.find('<<')
        if at > 0 and not any(mask[at:at+2]) and word[at+2:at+3] != '<' and (
                word[at-1] not in '<>&' and not re.fullmatch('[0-9]+', word[:at])):
            words.extend([word[:at], word[at:]])
        else:
            words.append(word)
    return words

def signal_argv(group):
    # Assignments, xargs provenance and wrapper peeling come from the shared
    # tokenizer so launch-guard and signal-guard see the same program.
    words = without_redirections(signal_split_heredocs(group))
    rest = words
    assigns, strings = [], []
    indirect = False
    while rest:
        indirect = indirect or wrapper_has_xargs(rest)
        peeled = strip_prefixes(rest)
        stop = len(rest) - len(peeled)
        for i, token in enumerate(rest[:stop]):
            match = SIG_ASSIGN.match(token)
            if match:
                assigns.append((match.group(1), match.group(2) == '+', match.group(3)))
            strings.extend(signal_launcher_strings(rest[i:]))
        rest = peeled
        if not rest:
            break
        program, base = signal_program(rest[0])
        match = SIG_ASSIGN.match(rest[0])
        if match:
            assigns.append((match.group(1), match.group(2) == '+', match.group(3)))
            rest = rest[1:]
            continue
        if base in SIG_KEYWORDS:
            rest = rest[1:]
            continue
        if base == 'coproc':
            rest = rest[1:]
            if len(rest) > 1 and rest[1] == '{':
                rest = rest[1:]
            continue
        if base == 'function':
            rest = rest[2:]
            continue
        rest = [program] + rest[1:]
        break
    strings.extend(signal_launcher_strings(rest))
    return rest, indirect, strings, assigns

def signal_kill(args, indirect, captures):
    if args and args[0] in ('-l', '-L', '--list', '--table'):
        return
    if indirect or not args:
        _block(SIGNAL_REASON)
    if args[0] in ('-s', '-n', '--signal'):
        args = args[2:]
    elif args[0].startswith('--signal='):
        args = args[1:]
    elif args[0].startswith('-') and args[0] != '--':
        # A lone negative argument (notably kill -1) is never an explicit PID.
        args = args[1:]
    if args and args[0] == '--':
        args = args[1:]
    # pid_t is signed 32-bit on worker hosts; prevent positive literals from
    # wrapping into a process group or the all-processes sentinel.
    def valid(pid):
        if pid in SIG_BANG:
            return True
        if pid.startswith(SIG_DOLLAR) and pid[1:].strip('{}') in captures:
            return True
        return re.fullmatch('[0-9]+', pid) and 0 < int(pid) <= 2147483647
    if not args or not all(valid(pid) for pid in args):
        _block(SIGNAL_REASON)

def signal_shell(args):
    return args and os.path.basename(signal_program(args[0])[0]) in SIG_SHELLS

def signal_stdin(args):
    # A shell reads its commands from stdin unless given -c or a script file.
    rest = args[1:]
    if any(a.startswith('-') and not a.startswith('--') and 'c' in a for a in rest):
        return False
    operands = signal_shell_operands(rest)
    return not operands or operands[0] in ('/dev/stdin', '/dev/fd/0', '-') or any(
        a.startswith('-') and not a.startswith('--') and 's' in a for a in rest)

def signal_script_file(args):
    _, base = signal_program(args[0])
    if not (base in SIG_SHELLS or base in ('source', '.')):
        return None
    if signal_cstrings(args[1:]):
        return None
    operands = signal_shell_operands(args[1:])
    return operands[0] if operands else None

def signal_text(text, depth, values=None, unknown=None, holders=None, scripts=None, ariths=None, written=None):
    # Inspect text that a shell will later read. Text that cannot be parsed is
    # still refused when it names the forbidden programs.
    try:
        signal_check(text, depth, values, unknown, holders, scripts, ariths, written)
    except ValueError:
        if SIG_NAME.search(text):
            _block(SIGNAL_REASON)

def signal_arith_names(text):
    names = {m.group(1) for m in SIG_ARITH_SET.finditer(text)}
    names.update(m.group(1) or m.group(2) for m in SIG_ARITH_STEP.finditer(text))
    return names

def signal_writes(group, args):
    # Names a command may rebind in the current shell, over-approximated.
    names = set()
    for k, word in enumerate(group):
        names.update(m.group(1) for m in SIG_PARAM.finditer(word))
        if SIG_IDENT.fullmatch(word) and k + 1 < len(group) and group[k+1].startswith(
                ('=', '+=', '-=', '*=', '/=', '%=', '++', '--')):
            names.add(word)
        names.update(m.group(1) or m.group(2) for m in [SIG_WORD_STEP.fullmatch(word)] if m)
    if args:
        _, base = signal_program(args[0])
        rest = args[1:]
        if base in SIG_BINDERS:
            names.update(w for w in rest if SIG_IDENT.fullmatch(w))
        elif base == 'printf':
            for k, word in enumerate(rest):
                if word == '-v' and k + 1 < len(rest):
                    names.add(rest[k+1])
                elif word.startswith('-v') and len(word) > 2:
                    names.add(word[2:])
        elif base in SIG_EVALUATORS:
            names.update(SIG_IDENT.findall(' '.join(rest)))
    return names

def signal_decode(text, operand=False, fallback=None):
    out, index = [], 0
    escapes = dict(zip('abefnrtv', map(chr, (7, 8, 27, 12, 10, 13, 9, 11))))
    escapes[SIG_BS] = SIG_BS
    while index < len(text):
        char = text[index]
        index += 1
        if char != SIG_BS or index == len(text):
            out.append(char)
            continue
        char = text[index]
        index += 1
        if char == 'c':
            return ''.join(out), True
        if char in escapes:
            out.append(escapes[char])
            continue
        digits, base, limit = '', 8, 3
        if char == 'x':
            base, limit = 16, 2
        elif char in '01234567':
            digits = '' if operand and char == '0' else char
            limit -= len(digits)
        else:
            # Unknown shell-specific escapes cannot hide a signalling word.
            if SIG_KILL_WORD.search(text if fallback is None else fallback):
                return 'pkill' + SIG_NL, False
            out.extend((SIG_BS, char))
            continue
        alphabet = '0123456789abcdef'[:base]
        while limit and index < len(text) and text[index].lower() in alphabet:
            digits += text[index]
            index += 1
            limit -= 1
        if digits:
            out.append(chr(int(digits, base)))
        elif SIG_KILL_WORD.search(text if fallback is None else fallback):
            return 'pkill' + SIG_NL, False
        else:
            out.extend((SIG_BS, char))
    return ''.join(out), False

def signal_output(group):
    content = getattr(group, 'body', '')
    if content:
        return content
    if here_string_body(group):
        return here_string_body(group) + SIG_NL
    args, _, _, _ = signal_argv(group)
    if not args:
        return ''
    _, base = signal_program(args[0])
    if base == 'echo':
        words = args[1:]
        expanding, newline = True, SIG_NL
        while words and re.fullmatch('-[neE]+', words[0]):
            if 'E' in words[0]:
                expanding = False
            if 'e' in words[0]:
                expanding = True
            newline = '' if 'n' in words[0] else newline
            words = words[1:]
        text = ' '.join(words)
        decoded, stopped = signal_decode(text, True) if expanding else (text, False)
        return decoded + ('' if stopped else newline)
    if base == 'printf' and len(args) > 1:
        if args[1] == '%s':
            return ''.join(args[2:])
        if args[1] == '%b':
            out = []
            for item in args[2:]:
                decoded, stopped = signal_decode(item, True, ''.join(args[2:]))
                out.append(decoded)
                if stopped:
                    break
            return ''.join(out)
        if args[1] == '%s' + SIG_BS + 'n':
            return ''.join(item + SIG_NL for item in args[2:])
        if '%' not in args[1]:
            return signal_decode(args[1])[0]
    if SIG_KILL_WORD.search(signal_chain_text(group)):
        return 'pkill' + SIG_NL
    return ''

def signal_redirects(group, writing):
    found = []
    group = signal_split_heredocs(group)
    for i, token in enumerate(group):
        pattern = '^(?:[0-9]+|&)?(>>|>[|]?)(.*)' if writing else '^(?:0)?(<)([^<].*|)'
        match = re.fullmatch(pattern, token)
        if not match and writing:
            # cat <<EOF>out.txt: the redirection follows the heredoc word.
            match = re.fullmatch('^(?:[0-9]+)?<<-?[^<>]+(>>|>[|]?)(.+)', token)
        if match:
            path = match.group(2) or (group[i+1] if i + 1 < len(group) else '')
            if path:
                found.append((path, match.group(1) == '>>'))
    return found

def signal_chain_text(upstream):
    parts = list(upstream)
    body = getattr(upstream, 'body', '') or ''
    here = here_string_body(upstream)
    return ' '.join(parts) + ' ' + body + ' ' + here

def signal_check(source, depth=0, inherited=None, unknown=None, holders=None, scripts=None, ariths=None, written=None):
    if depth > 40:
        _block(SIGNAL_REASON + ' Shell nesting is too deep to inspect safely.')
    if holders is None:
        holders = []
    if scripts is None:
        scripts = set()
    if ariths is None:
        ariths = []
    if written is None:
        written = {}
    scan = extract_substitutions(source, holders, scripts, ariths)
    records = command_records(scan['out'])
    if any(group.error for group in records):
        raise ValueError(next(group.error for group in records if group.error))
    groups = [group for group in records if group]
    parsed = [signal_argv(group) for group in groups]

    # Provenance is decided over the whole command: a name holds a captured PID
    # only if every write to it is a pure assignment of the background PID.
    values = {name: set(items) for name, items in (inherited or {}).items()}
    writes, first, rebound = {}, {}, set()
    for index, group in enumerate(groups):
        args, _, _, assigns = parsed[index]
        for name, plus, value in assigns:
            if plus:
                rebound.add(name)
            pure = not args and not plus and value in SIG_BANG
            writes.setdefault(name, []).append(pure)
            if not plus:
                values.setdefault(name, set()).add(value)
            if pure:
                first.setdefault(name, index)
        if args and signal_program(args[0])[1] in SIG_DECLARERS:
            for word in args[1:]:
                match = SIG_ASSIGN.match(word)
                if match:
                    writes.setdefault(match.group(1), []).append(False)
                    if not match.group(2):
                        values.setdefault(match.group(1), set()).add(match.group(3))
                elif SIG_IDENT.fullmatch(word):
                    writes.setdefault(word, []).append(False)
                    rebound.add(word)
        rebound |= signal_writes(group, args)
    for text in scan['ariths']:
        rebound |= signal_arith_names(text)
    # Check every raw identifier occurrence, including nested commands and data.
    # Arithmetic writes and namerefs revoke provenance without a syntax allowlist.
    def pure_occurrences(name):
        occurrence = re.compile('(?<![A-Za-z0-9_])' + re.escape(name) + '(?![A-Za-z0-9_])')
        for match in occurrence.finditer(source):
            before, after = source[:match.start()], source[match.end():]
            capture = any(after.startswith('=' + bang) for bang in SIG_BANG)
            plain = before.endswith(SIG_DOLLAR) or (
                before.endswith(SIG_DOLLAR + '{') and after.startswith('}'))
            if not capture and not plain:
                return False
        return True
    qualified = {name for name in first if name not in rebound and all(writes[name])
                 and pure_occurrences(name)}
    mentions = SIG_NAME.search(source)
    unknown = dict(unknown or {})
    for name in rebound:
        unknown[name] = unknown.get(name, False) or bool(mentions)

    def inspect(text, hops=0):
        # Expand recorded command-string variables without executing the shell.
        if hops >= 8:
            _block(SIGNAL_REASON + ' Variable indirection is too deep to inspect safely.')
        variable = re.search(re.escape(SIG_DOLLAR) + '([{]?[A-Za-z_][A-Za-z0-9_]*[}]?)', text)
        if variable:
            name = variable.group(1).strip('{}')
            if name in values and not unknown.get(name, False):
                for value in sorted(values[name]):
                    inspect(text[:variable.start()] + value + text[variable.end():], hops + 1)
                return
        signal_text(text, depth+1, values, unknown, holders, scripts, ariths, written)

    def input_words(text):
        rewritten = extract_substitutions(text, holders, scripts, ariths)['out']
        for group in command_records(rewritten):
            if not group:
                continue
            for word in group:
                inspect(word)

    def lookup_holder(index):
        if index < 0 or index >= len(holders):
            _block(SIGNAL_REASON)
        return holders[index]

    def dispatch(group, args, indirect, chain, hops, captures):
        if not args:
            return
        program = args[0]
        variables = list(SIG_VARIABLE.finditer(program))
        for variable in variables:
            name = variable.group(1) or variable.group(2)
            if unknown.get(name, False):
                _block(SIGNAL_REASON)
            if name in values:
                if hops >= 8:
                    _block(SIGNAL_REASON + ' Variable indirection is too deep to inspect safely.')
                for value in sorted(values[name]):
                    expanded = program[:variable.start()] + value + program[variable.end():]
                    # Conservatively inspect field splitting as well when quote provenance
                    # is unavailable after recursive expansion.
                    positions = [i for i, word in enumerate(group) if word == program]
                    masks = getattr(group, 'literal_chars', [])
                    quoted = bool(positions and masks and all(
                        masks[positions[0]][variable.start():variable.end()]))
                    shapes = [[expanded]] if quoted else [expanded.split()]
                    if not positions or not masks or getattr(group, 'quote_segments', None) is None:
                        shapes.append([expanded])
                    for fields in shapes:
                        inner, inner_indirect, inner_strings, _ = signal_argv(fields + args[1:])
                        for string in inner_strings:
                            inspect(string)
                        dispatch(group, inner, indirect or inner_indirect, chain, hops + 1, captures)
                return
        if variables:
            for string in signal_cstrings(args[1:]):
                inspect(string)
        program, base = signal_program(program)
        args = [program] + args[1:]
        for match in SIG_HOLDER.finditer(program):
            if SIG_NAME.search(lookup_holder(int(match.group(1)))):
                _block(SIGNAL_REASON)
        parts = SIG_VARIABLE.sub(chr(0), SIG_HOLDER.sub(chr(0), program)).split(chr(0))
        if len(parts) > 1 and any(len(p) > 1 and (p in 'pkill' or p in 'killall' or p in 'kill') for p in parts):
            _block(SIGNAL_REASON)
        if base in ('pkill', 'killall'):
            _block(SIGNAL_REASON + ' Refused command: ' + base + '.')
        if base == 'kill':
            signal_kill(args[1:], indirect, captures)
        if base == 'find':
            for k, word in enumerate(args):
                if word in ('-exec', '-execdir', '-ok', '-okdir'):
                    command = []
                    for item in args[k+1:]:
                        if item in (';', '+'):
                            break
                        command.append(item)
                    inner, inner_indirect, inner_strings, _ = signal_argv(command)
                    for string in inner_strings:
                        inspect(string)
                    dispatch(group, inner, inner_indirect, chain, hops, captures)
        if base == 'eval':
            inspect(' '.join(args[1:]))
        if base == 'trap':
            action, i = args[1:], 0
            while i < len(action):
                if action[i] == '--':
                    i += 1
                    break
                if action[i] in ('-p', '-l'):
                    action = []
                    break
                if action[i].startswith('-'):
                    i += 1
                    continue
                break
            if i < len(action):
                inspect(action[i])
        if signal_shell(args) or base in ('source', '.'):
            for word in args[1:]:
                for match in SIG_HOLDER.finditer(word):
                    index = int(match.group(1))
                    if index in scan['scripts']:
                        input_words(lookup_holder(index))
            path = signal_script_file(args)
            if path:
                inspect_written(path)
        inspect_written(program)
        if signal_shell(args):
            if signal_stdin(args):
                for path, _ in signal_redirects(group, False):
                    inspect_written(path)
                if getattr(group, 'body', ''):
                    inspect(group.body)
                here = here_string_body(group)
                if here:
                    inspect(here)
                if chain:
                    for upstream in chain:
                        if SIG_KILL_WORD.search(signal_chain_text(upstream)):
                            _block(SIGNAL_REASON)
                        if getattr(upstream, 'body', ''):
                            inspect(upstream.body)
                        here = here_string_body(upstream)
                        if here:
                            inspect(here)
                        for word in upstream:
                            inspect(word)
            for string in signal_cstrings(args[1:]):
                inspect(string)

    def inspect_written(path):
        key = os.path.normpath(path)
        for content in tuple(written.get(key, set())):
            inspect(content)

    for index, group in enumerate(groups):
        args, indirect, strings, _ = parsed[index]
        content = getattr(group, 'body', '') or (
            here_string_body(group) + SIG_NL if here_string_body(group) else '')
        targets = signal_redirects(group, True)
        if args:
            _, base = signal_program(args[0])
            if base in ('echo', 'printf'):
                content = signal_output(group)
            if base == 'tee':
                targets.extend((word, '-a' in args or '--append' in args)
                               for word in args[1:] if not word.startswith('-'))
            if not content:
                content = ''.join(signal_output(upstream)
                                  for upstream in getattr(group, 'pipe_sources', []))
            if not content and SIG_KILL_WORD.search(' '.join(args)):
                content = 'pkill' + SIG_NL
        for target, append in targets:
            key = os.path.normpath(target)
            variants = written.setdefault(key, set())
            additions = {prior + content for prior in variants | {''}} if append else {content}
            variants.update(additions)

    # Deferred actions and conditional writes can observe any recorded variant.
    for index, group in enumerate(groups):
        args, indirect, strings, _ = parsed[index]
        for string in strings:
            inspect(string)
        captures = {name for name in qualified if first[name] < index}
        dispatch(group, args, indirect, getattr(group, 'pipe_sources', []), 0, captures)

    # Substitutions run before or during the commands that contain them, so
    # check them once every script written by this command is on record.
    for substitution in scan['subs']:
        signal_check(substitution, depth+1, values, unknown, holders, scripts, ariths, written)
    for group in groups:
        if group.body and group.expanding_body:
            nested = extract_substitutions(group.body, holders, scripts, ariths, 0, None, 'literal')
            for substitution in nested['subs']:
                signal_check(substitution, depth+1, values, unknown, holders, scripts, ariths, written)

try:
    signal_check(cmd)
except Exception as error:
    _block(SIGNAL_REASON + ' Cannot safely parse this shell command: ' + str(error))
