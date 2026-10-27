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
SIG_BLANKS = ' ' + SIG_TAB + SIG_NL + chr(13) + ';&|('
SIG_IDENT = re.compile('[A-Za-z_][A-Za-z0-9_]*')
SIG_ASSIGN = re.compile('^([A-Za-z_][A-Za-z0-9_]*)([+]?)=(.*)', re.S)
SIG_NAME = re.compile('(?<![A-Za-z_])(pkill|killall)(?![A-Za-z_])')
SIG_HOLDER = re.compile('__indirect_pid_([0-9]+)__')
SIG_PARAM = re.compile(re.escape(SIG_DOLLAR) + '[{]([A-Za-z_][A-Za-z0-9_]*)(?![A-Za-z0-9_}])')
SIG_ARITH_SET = re.compile('([A-Za-z_][A-Za-z0-9_]*)[ ]*(?:[-+*/%&|^]|<<|>>)?=(?!=)')
SIG_ARITH_STEP = re.compile('([A-Za-z_][A-Za-z0-9_]*)[ ]*(?:[+][+]|--)|(?:[+][+]|--)[ ]*([A-Za-z_][A-Za-z0-9_]*)')
SIG_WORD_STEP = re.compile('([A-Za-z_][A-Za-z0-9_]*)(?:[+][+]|--)|[+][+]([A-Za-z_][A-Za-z0-9_]*)')
SIG_SHELLS = ('bash', 'sh', 'zsh', 'dash', 'ksh')
SIG_DECLARERS = ('export', 'declare', 'typeset', 'local', 'readonly')
SIG_BINDERS = ('read', 'mapfile', 'readarray', 'getopts', 'for', 'select', 'unset', 'wait')
SIG_EVALUATORS = ('let', 'eval', 'source', '.')
SIG_KEYWORDS = ('!', '{', '}', 'then', 'do', 'if', 'elif', 'while', 'until', 'else')
SIG_LAUNCHERS = ('timeout', 'builtin', 'busybox', 'nice', 'ionice', 'doas',
                 'flock', 'chrt', 'taskset', 'script')
SIG_POSITIONAL = {'timeout': 1, 'flock': 1, 'chrt': 1, 'taskset': 1, 'script': 1}

def signal_ansi(text):
    # Decode the body of an ANSI-C quoted word so that its spelling cannot hide a name.
    simple = {'n': SIG_NL, 't': SIG_TAB, 'r': chr(13), 'a': chr(7), 'b': chr(8), 'e': chr(27),
              'E': chr(27), 'f': chr(12), 'v': chr(11), SIG_BS: SIG_BS, SIG_SQ: SIG_SQ,
              SIG_DQ: SIG_DQ, '?': '?'}
    widths = {'x': 2, 'u': 4, 'U': 8}
    out, i = [], 0
    while i < len(text):
        c = text[i]
        if c != SIG_BS or i + 1 >= len(text):
            out.append(c)
            i += 1
            continue
        d = text[i+1]
        if d in simple:
            out.append(simple[d])
            i += 2
        elif d in widths:
            digits = re.match('[0-9a-fA-F]*', text[i+2:i+2+widths[d]]).group()
            if digits:
                out.append(chr(int(digits, 16)))
            else:
                out.append(c + d)
            i += 2 + len(digits)
        elif d in '01234567':
            digits = re.match('[0-7]{1,3}', text[i+1:]).group()
            out.append(chr(int(digits, 8) % 256))
            i += 1 + len(digits)
        else:
            out.append(c + d)
            i += 2
    return ''.join(out)

def signal_word_end(source, i):
    # Read one shell word (a heredoc delimiter) starting at i. Returns the word with
    # its quoting removed, whether any quoting was present, and the end offset.
    n, value, quoted = len(source), [], False
    while i < n and source[i] not in ' ;&|()<>' + SIG_TAB + SIG_NL + chr(13):
        c = source[i]
        if c == SIG_DOLLAR and i + 1 < n and source[i+1] in (SIG_SQ, SIG_DQ):
            i += 1
        elif c == SIG_BS:
            quoted = True
            if i + 1 < n:
                value.append(source[i+1])
            i += 2
        elif c in (SIG_SQ, SIG_DQ):
            end = source.find(c, i + 1)
            if end < 0:
                raise ValueError('unterminated quote in heredoc delimiter')
            value.append(source[i+1:end])
            quoted = True
            i = end + 1
        else:
            value.append(c)
            i += 1
    return ''.join(value), quoted, i

def signal_leading(text):
    # Bash reads a dollar-double-paren as arithmetic only when the inner paren that
    # opens the text closes at its very end; anything else is a subshell command.
    if not text.startswith('('):
        return False
    try:
        return signal_scan(text, 1, ')', 'probe')['end'] == len(text) - 1
    except ValueError:
        return False

def signal_scan(source, start=0, stop=None, kind='command'):
    # One quote-aware pass that separates executable substitutions from ordinary
    # quoted arguments, drops comments and heredoc bodies, and keeps a nonnumeric
    # placeholder at each call site so kill cannot accept their output.
    # kind: command (everything), arith (quotes and substitutions),
    # probe (quotes only) or literal (substitutions only, for heredoc bodies).
    quotes, subs_on, command = kind != 'literal', kind != 'probe', kind == 'command'
    n = len(source)
    out, subs, holders, heredocs, ariths = [], [], [], [], []
    pending, quote, depth, found = [], None, 0, False
    word_start, mark, arith_until, i = True, 0, 0, start
    while i < n:
        c = source[i]
        if c == SIG_BS and quote != SIG_SQ:
            out.append(source[i:i+2])
            i += 2
            word_start = False
            continue
        if command and quote is None and word_start and c == '#':
            end = source.find(SIG_NL, i)
            i = n if end < 0 else end
            continue
        if stop == SIG_BT and c == SIG_BT:
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
        if quotes and c in (SIG_SQ, SIG_DQ) and quote in (None, c):
            quote = None if quote else c
            out.append(c)
            i += 1
            word_start = False
            continue
        if quotes and quote is None and c == SIG_DOLLAR and source[i+1:i+2] == SIG_DQ:
            i += 1
            word_start = False
            continue
        if quotes and quote is None and c == SIG_DOLLAR and source[i+1:i+2] == SIG_SQ:
            end = i + 2
            while end < n and source[end] != SIG_SQ:
                end += 2 if source[end] == SIG_BS else 1
            if end >= n:
                raise ValueError('unterminated ANSI-C quote')
            decoded = signal_ansi(source[i+2:end]).replace(SIG_SQ, SIG_SQ + SIG_BS + SIG_SQ + SIG_SQ)
            out.append(SIG_SQ + decoded + SIG_SQ)
            i = end + 1
            word_start = False
            continue
        if subs_on and quote != SIG_SQ and (c == SIG_BT or source.startswith(SIG_DOLLAR + '(', i)):
            if c == SIG_BT:
                j = signal_scan(source, i + 1, SIG_BT)['end']
                text = source[i+1:j]
                subs.append(text)
            else:
                s, arithmetic = i + 2, False
                if source.startswith('((', i + 1):
                    j = signal_scan(source, s, ')', 'probe')['end']
                    text = source[s:j]
                    arithmetic = signal_leading(text)
                if arithmetic:
                    subs.extend(signal_scan(text, 0, None, 'arith')['subs'])
                    ariths.append(text)
                else:
                    j = signal_scan(source, s, ')')['end']
                    text = source[s:j]
                    subs.append(text)
            holders.append(text)
            out.append('__indirect_pid_' + str(len(holders) - 1) + '__')
            i = j + 1
            word_start = False
            continue
        if command and quote is None and word_start and i >= arith_until and source.startswith('((', i):
            # An arithmetic command: record what it assigns, and keep its text
            # visible while suspending heredoc detection for the shifts inside it.
            try:
                j = signal_scan(source, i + 2, ')', 'probe')['end']
                if source[j+1:j+2] == ')':
                    ariths.append(source[i+2:j])
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
            while j < n and source[j] in ' ' + SIG_TAB:
                j += 1
            delimiter, quoted, end = signal_word_end(source, j)
            if not delimiter:
                raise ValueError('unrecognised heredoc delimiter')
            pending.append((delimiter, strip, quoted))
            out.append(source[i:end])
            i = end
            word_start = False
            continue
        if command and quote is None and c == SIG_NL:
            out.append(SIG_NL)
            i += 1
            header = ''.join(out[mark:])
            for delimiter, strip, quoted in pending:
                body = []
                while True:
                    if i >= n:
                        raise ValueError('unterminated heredoc')
                    end = source.find(SIG_NL, i)
                    end = n if end < 0 else end
                    line = source[i:end]
                    i = end + 1
                    if (line.lstrip(SIG_TAB) if strip else line) == delimiter:
                        break
                    body.append(line)
                heredocs.append((header, SIG_NL.join(body), quoted))
            pending = []
            mark = len(out)
            word_start = True
            continue
        out.append(c)
        i += 1
        if quote is None:
            word_start = c in SIG_BLANKS
    if stop is not None and not found:
        raise ValueError('unterminated command substitution')
    if pending:
        raise ValueError('unterminated heredoc')
    return {'out': ''.join(out), 'subs': subs, 'holders': holders, 'heredocs': heredocs,
            'ariths': ariths, 'end': i}

def signal_cstrings(words):
    # Shell command strings handed to a launcher with -c or --command.
    found = []
    for i, word in enumerate(words):
        if word.startswith('--command='):
            found.append(word.split('=', 1)[1])
        elif i + 1 < len(words) and (word == '--command' or (
                word.startswith('-') and not word.startswith('--') and 'c' in word)):
            found.append(words[i+1])
    return found

def signal_argv(group):
    # Retain xargs provenance and consume wrapper options with operands before
    # finding the program, extending the common tokenizer prefix handling.
    # Redirections can precede the executable as well as follow its arguments.
    # Returns the command words, whether xargs fed it, shell strings that a
    # launcher will run, and the assignments seen in leading positions.
    words, i = [], 0
    redirects = ('<', '>', '>>', '<<', '<<<', '<>', '>&', '<&', '&>', '&>>', '>|')
    while i < len(group):
        if not words and group[i].isdigit() and i+1 < len(group) and group[i+1] in redirects:
            i += 1
        if group[i] in redirects:
            i += 2
        else:
            words.append(group[i])
            i += 1
    group = words
    indirect, strings, assigns = False, [], []
    while group:
        base = os.path.basename(group[0])
        match = SIG_ASSIGN.match(group[0])
        if match:
            assigns.append((match.group(1), match.group(2) == '+', match.group(3)))
            group = group[1:]
            continue
        if base in SIG_KEYWORDS:
            group = group[1:]
            continue
        if base == 'coproc':
            group = group[1:]
            if len(group) > 1 and group[1] == '{':
                group = group[1:]
            continue
        if base == 'function':
            group = group[2:]
            continue
        if base == 'su':
            return [], indirect, strings + signal_cstrings(group[1:]), assigns
        if base == 'watch':
            group = group[1:]
            while group and group[0].startswith('-'):
                group = group[2:] if group[0] in ('-n', '--interval') else group[1:]
            return [], indirect, strings + [' '.join(group)], assigns
        if base not in WRAPPERS and base not in SIG_LAUNCHERS:
            break
        indirect |= base == 'xargs'
        group = group[1:]
        if base in ('flock', 'script'):
            strings.extend(signal_cstrings(group))
        if base == 'command' and group and group[0] in ('-v', '-V'):
            return [], indirect, strings, assigns
        takes_value = {
            'sudo': ('-u', '-g', '-h', '-p', '-C', '-T', '-R', '-D', '-r', '-t', '-U',
                     '--user', '--group', '--host', '--prompt', '--chdir', '--chroot',
                     '--role', '--type', '--other-user', '--close-from', '--command-timeout'),
            'env': ('-u', '-C', '-a', '--unset', '--chdir', '--argv0'),
            'exec': ('-a',),
            'nice': ('-n', '--adjustment'),
            'ionice': ('-c', '-n', '-p', '-P', '-u', '--class', '--classdata', '--pid', '--pgid', '--uid'),
            'doas': ('-u', '-C'),
            'xargs': ('-I', '-J', '-n', '-P', '-s', '-E', '-L', '-d', '--replace', '--max-args', '--max-procs'),
            'timeout': ('-s', '-k', '--signal', '--kill-after'),
            'caffeinate': ('-t', '-w'),
            'stdbuf': ('-i', '-o', '-e'),
            'flock': ('-w', '--timeout', '-E', '--conflict-exit-code', '-c', '--command'),
            'chrt': ('-T', '-P', '-D'),
            'taskset': ('-c', '--cpu-list'),
            'script': ('-t', '-F', '-c', '--command', '-I', '-O', '-B', '-T', '-E', '-m',
                       '--log-in', '--log-out', '--log-io', '--log-timing', '--timing',
                       '--output-limit'),
        }.get(base, ())
        positional = SIG_POSITIONAL.get(base, 0)
        while group and group[0].startswith('-'):
            option = group[0]
            if base == 'env' and option in ('-S', '--split-string'):
                group = shlex.split(group[1]) + group[2:]
                break
            if base == 'env' and option.startswith('--split-string='):
                group = shlex.split(option.split('=', 1)[1]) + group[1:]
                break
            if base == 'env' and option.startswith('-S'):
                group = shlex.split(option[2:]) + group[1:]
                break
            if base == 'taskset' and (option == '-c' or option.startswith('--cpu-list')):
                positional = 0
            group = group[2:] if option in takes_value else group[1:]
            if option == '--':
                break
        group = group[positional:]
    return group, indirect, strings, assigns

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
    return args and os.path.basename(args[0]) in SIG_SHELLS

def signal_stdin(args):
    # A shell reads its commands from stdin unless given -c or a script file.
    rest, i = args[1:], 0
    if any(a.startswith('-') and not a.startswith('--') and 'c' in a for a in rest):
        return False
    while i < len(rest):
        if rest[i] in ('-o', '+o', '-O', '+O', '--rcfile', '--init-file'):
            i += 2
        elif rest[i][:1] in ('-', '+'):
            i += 1
        else:
            return any(a.startswith('-') and not a.startswith('--') and 's' in a for a in rest)
    return True

def signal_text(text, depth):
    # Inspect text that a shell will later read. Text that cannot be parsed is
    # still refused when it names the forbidden programs.
    try:
        signal_check(text, depth)
    except ValueError:
        if SIG_NAME.search(text):
            _block(SIGNAL_REASON)

def signal_groups(source):
    # Split into command groups like the shared tokenizer, but also report
    # whether each group is fed by a pipe and which groups feed it.
    lex = shlex.shlex(source, posix=True, punctuation_chars=';&|()<>' + SIG_NL)
    lex.whitespace = ' ' + SIG_TAB + chr(13)
    lex.whitespace_split = True
    lex.commenters = ''
    tokens = [p for t in lex for p in (re.split('([()])', t) if all(c in ';&|()<>' + SIG_NL for c in t) else [t]) if p]
    groups, current, piped, pipeline = [], [], False, []
    for token in tokens + [';']:
        if token in DELIMS or all(c in ';&|()' + SIG_NL for c in token):
            if current:
                groups.append((current, piped, pipeline if piped else []))
                pipeline = pipeline + [current] if piped else [current]
                current, piped = [], False
            if token in ('|', '|&'):
                piped = True
            elif token.strip(SIG_NL):
                piped = False
        else:
            current.append(token)
    return groups

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
        base, rest = os.path.basename(args[0]), args[1:]
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

def signal_mentions(groups):
    return any(w.split() and os.path.basename(w.split()[0]) in ('pkill', 'killall')
               for g, _, _ in groups for w in g)

def signal_check(source, depth=0):
    if depth > 40:
        _block(SIGNAL_REASON + ' Shell nesting is too deep to inspect safely.')
    source = source.replace(SIG_BS + SIG_NL, '')
    scan = signal_scan(source)
    holders = scan['holders']
    for substitution in scan['subs']:
        signal_check(substitution, depth+1)
    for header, body, quoted in scan['heredocs']:
        if any(signal_shell(signal_argv(g)[0]) for g, _, _ in signal_groups(header)):
            signal_check(body, depth+1)
        elif not quoted:
            for substitution in signal_scan(body, 0, None, 'literal')['subs']:
                signal_check(substitution, depth+1)
    groups = signal_groups(scan['out'])
    parsed = [signal_argv(g) for g, _, _ in groups]

    # Provenance is decided over the whole command: a name holds a captured PID
    # only if every write to it is a pure assignment of the background PID.
    values, writes, first, rebound = {}, {}, {}, set()
    for index, (group, _, _) in enumerate(groups):
        args, _, _, assigns = parsed[index]
        for name, plus, value in assigns:
            pure = not args and not plus and value in SIG_BANG
            writes.setdefault(name, []).append(pure)
            if not plus:
                values.setdefault(name, set()).add(value)
            if pure:
                first.setdefault(name, index)
        if args and os.path.basename(args[0]) in SIG_DECLARERS:
            for word in args[1:]:
                match = SIG_ASSIGN.match(word)
                if match:
                    writes.setdefault(match.group(1), []).append(False)
                    if not match.group(2):
                        values.setdefault(match.group(1), set()).add(match.group(3))
                elif SIG_IDENT.fullmatch(word):
                    writes.setdefault(word, []).append(False)
        rebound |= signal_writes(group, args)
    for text in scan['ariths']:
        rebound |= signal_arith_names(text)
    qualified = {name for name in first if name not in rebound and all(writes[name])}

    def dispatch(group, args, indirect, chain, hops, captures):
        if not args:
            return
        program = args[0]
        if program.startswith(SIG_DOLLAR):
            name = program[1:].strip('{}')
            if name in values:
                if hops >= 8:
                    _block(SIGNAL_REASON + ' Variable indirection is too deep to inspect safely.')
                for value in sorted(values[name]):
                    inner, inner_indirect, inner_strings, _ = signal_argv(value.split() + args[1:])
                    for string in inner_strings:
                        signal_check(string, depth+1)
                    dispatch(group, inner, indirect or inner_indirect, chain, hops + 1, captures)
                return
            if signal_mentions(groups):
                _block(SIGNAL_REASON)
        base = os.path.basename(program)
        for match in SIG_HOLDER.finditer(program):
            if SIG_NAME.search(holders[int(match.group(1))]):
                _block(SIGNAL_REASON)
        parts = SIG_HOLDER.sub(chr(0), program).split(chr(0))
        if len(parts) > 1 and any(len(p) > 1 and (p in 'pkill' or p in 'killall') for p in parts):
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
                        signal_check(string, depth+1)
                    dispatch(group, inner, inner_indirect, chain, hops, captures)
        if base == 'eval':
            signal_check(' '.join(args[1:]), depth+1)
        if signal_shell(args):
            if signal_stdin(args):
                for k, word in enumerate(group):
                    if word == '<<<' and k + 1 < len(group):
                        signal_text(group[k+1], depth+1)
                for upstream in chain:
                    for word in upstream:
                        signal_text(word, depth+1)
            for k, arg in enumerate(args[1:], 1):
                if arg.startswith('-') and not arg.startswith('--') and 'c' in arg:
                    if k+1 < len(args):
                        signal_check(args[k+1], depth+1)
                    break

    for index, (group, _, chain) in enumerate(groups):
        args, indirect, strings, _ = parsed[index]
        for string in strings:
            signal_check(string, depth+1)
        captures = {name for name in qualified if first[name] < index}
        dispatch(group, args, indirect, chain, 0, captures)

try:
    signal_check(cmd)
except Exception as error:
    _block(SIGNAL_REASON + ' Cannot safely parse this shell command: ' + str(error))
