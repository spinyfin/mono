# Embedded in the always-on launch guard for every driver. Reuse its shell
# tokenizer; this fragment must contain no shell double quotes, backticks,
# dollars or backslashes because the enclosing command uses double quotes.
SIGNAL_REASON = ('Blocked unsafe process signalling: pkill/killall select processes by '
                 'name or pattern, and kill with indirect PIDs, process groups or broad '
                 'targets can terminate other workers. Stop only processes you started '
                 'yourself using explicit positive numeric PIDs (for example, the PID '
                 'captured from ' + chr(36) + '! when starting your process), or wait '
                 'for or time out your own command.')

def signal_comment(source, i):
    return source[i] == '#' and (i == 0 or source[i-1] in ' ;&|()<>'+chr(9)+chr(10))

def signal_body_end(source, start, newline):
    delimiter, _ = signal_heredoc(source[start:newline])
    if delimiter is None:
        return newline
    end = newline + 1
    while end < len(source):
        next_line = source.find(chr(10), end)
        if next_line < 0:
            next_line = len(source)
        if source[end:next_line].lstrip(chr(9)) == delimiter:
            return next_line
        end = next_line + 1
    raise ValueError('unterminated heredoc')

def signal_substitutions(source, literal_quotes=False):
    # Separate executable substitutions from ordinary quoted arguments. Keep a
    # nonnumeric placeholder at the call site so kill cannot accept their output.
    output, substitutions = [], []
    quote = None
    i = 0
    while i < len(source):
        c = source[i]
        if not literal_quotes and quote is None and signal_comment(source, i):
            end = source.find(chr(10), i)
            i = len(source) if end < 0 else end
            continue
        if not literal_quotes and quote is None and c == chr(10):
            header = ''.join(output).rsplit(chr(10), 1)[-1]
            tail = header + source[i:]
            end = i + signal_body_end(tail, 0, len(header)) - len(header)
            output.append(source[i:end+1])
            i = end + 1
            continue
        if c == chr(92) and quote != chr(39):
            output.append(source[i:i+2])
            i += 2
            continue
        if not literal_quotes and c == chr(39) and quote != chr(34):
            quote = None if quote else c
        elif not literal_quotes and c == chr(34) and quote != chr(39):
            quote = None if quote else c
        elif quote != chr(39) and (c == chr(96) or source[i:i+2] == chr(36)+'('):
            backtick = c == chr(96)
            start = i + (1 if backtick else 2)
            j, depth, inner_quote = start, 1, None
            inner_start = start
            arithmetic = source[i:i+3] == chr(36)+'(('
            while j < len(source):
                t = source[j]
                if inner_quote is None and signal_comment(source, j):
                    end = source.find(chr(10), j)
                    j = len(source) if end < 0 else end
                    continue
                if inner_quote is None and t == chr(10):
                    j = (j if arithmetic else signal_body_end(source, inner_start, j)) + 1
                    inner_start = j
                    continue
                if t == chr(92):
                    j += 2
                    continue
                if backtick and t == chr(96):
                    break
                if not backtick:
                    if t in (chr(39), chr(34)):
                        if inner_quote is None:
                            inner_quote = t
                        elif inner_quote == t:
                            inner_quote = None
                    elif inner_quote is None:
                        if t == '(':
                            depth += 1
                        elif t == ')':
                            depth -= 1
                            if depth == 0:
                                break
                j += 1
            if j == len(source):
                raise ValueError('unterminated command substitution')
            content = source[start:j]
            if arithmetic:
                substitutions.extend(signal_substitutions(content)[1])
            else:
                substitutions.append(content)
            output.append('__indirect_pid__')
            i = j + 1
            continue
        output.append(c)
        i += 1
    return ''.join(output), substitutions

def signal_argv(group):
    # Retain xargs provenance and consume wrapper options with operands before
    # finding the program, extending the common tokenizer prefix handling.
    # Redirections can precede the executable as well as follow its arguments.
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
    indirect = False
    while group:
        base = os.path.basename(group[0])
        if ASSIGNMENT_RE.match(group[0]) or base in ('!', '{', '}', 'then', 'do', 'if', 'elif', 'while', 'until', 'else', 'coproc'):
            group = group[1:]
            continue
        if base == 'function':
            group = group[2:]
            continue
        if base not in WRAPPERS and base not in ('timeout', 'builtin', 'busybox', 'nice', 'ionice', 'doas'):
            break
        indirect |= base == 'xargs'
        group = group[1:]
        if base == 'command' and group and group[0] in ('-v', '-V'):
            return [], indirect
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
        }.get(base, ())
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
            group = group[2:] if option in takes_value else group[1:]
            if option == '--':
                break
        if base == 'timeout':
            group = group[1:]
    return group, indirect

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
        if pid in (chr(36)+'!', chr(36)+'{!}'):
            return True
        if pid.startswith(chr(36)) and pid[1:].strip('{}') in captures:
            return True
        return re.fullmatch('[0-9]+', pid) and 0 < int(pid) <= 2147483647
    if not args or not all(valid(pid) for pid in args):
        _block(SIGNAL_REASON)

def signal_heredoc(line):
    # Only a real redirection introduces a heredoc, never quoted documentation
    # such as echo '<<EOF'. Reuse the shared delimiter reader at that position.
    quote, i = None, 0
    while i < len(line):
        c = line[i]
        if c == chr(92) and quote != chr(39):
            i += 2
            continue
        if c in (chr(39), chr(34)):
            if quote is None:
                quote = c
            elif quote == c:
                quote = None
        elif quote is None and signal_comment(line, i):
            break
        elif quote is None and line[i:i+2] == '<<':
            if line[i:i+3] == '<<<':
                i += 3
                continue
            rest = line[i+2:].lstrip('-').lstrip()
            delim = heredoc_delim(line[i:])
            if delim is None:
                raise ValueError('unrecognised heredoc delimiter')
            return delim, rest.startswith((chr(39), chr(34)))
        i += 1
    return None, False

def signal_shell(args):
    return args and os.path.basename(args[0]) in ('bash', 'sh', 'zsh', 'dash', 'ksh')

def signal_source(source, depth):
    lines, body, heredoc_end, quoted, shell_body = [], [], None, False, False
    for line in source.split(chr(10)):
        if heredoc_end is not None:
            if line.lstrip(chr(9)) == heredoc_end:
                data = chr(10).join(body)
                if shell_body:
                    signal_check(data, depth+1)
                elif not quoted:
                    for substitution in signal_substitutions(data, literal_quotes=True)[1]:
                        signal_check(substitution, depth+1)
                heredoc_end = None
                body = []
            else:
                body.append(line)
            continue
        heredoc_end, quoted = signal_heredoc(line)
        if heredoc_end:
            shell_body = any(signal_shell(signal_argv(g)[0]) for g in command_groups(line, nested=True))
        lines.append(line)
    if heredoc_end is not None:
        raise ValueError('unterminated heredoc')
    return chr(10).join(lines)

def signal_check(source, depth=0):
    if depth > 40:
        _block(SIGNAL_REASON + ' Shell nesting is too deep to inspect safely.')
    source = source.replace(chr(92)+chr(10), '')
    source, substitutions = signal_substitutions(source)
    source = signal_source(source, depth)
    for substitution in substitutions:
        signal_check(substitution, depth+1)
    variables, assignments, captures = {}, set(), set()
    groups = command_groups(source, nested=True)
    for group in groups:
        args, indirect = signal_argv(group)
        # Only assignment prefixes change shell variables; arguments to echo
        # or another program cannot establish or overwrite PID provenance.
        for word in group:
            if not ASSIGNMENT_RE.match(word):
                break
            name, value = word.split('=', 1)
            captures.discard(name)
            if not args and name not in assignments and value in (chr(36)+'!', chr(36)+'{!}'):
                captures.add(name)
            assignments.add(name)
            variables[name] = value
        if not args:
            continue
        program = args[0]
        if program.startswith(chr(36)):
            program = variables.get(program[1:].strip('{}'), program)
        base = os.path.basename(program)
        if base == '__indirect_pid__' and any(re.search(r'(?<![A-Za-z_])(pkill|killall)(?![A-Za-z_])', sub) for sub in substitutions):
            _block(SIGNAL_REASON)
        if base in ('pkill', 'killall'):
            _block(SIGNAL_REASON + ' Refused command: ' + base + '.')
        if base == 'kill':
            signal_kill(args[1:], indirect, captures)
        if signal_shell(args):
            if '|' in source and any(re.search(r'(?<![A-Za-z_])(pkill|killall)(?![A-Za-z_])', word) for g in groups for word in g):
                _block(SIGNAL_REASON)
            for i, arg in enumerate(args[1:], 1):
                if arg.startswith('-') and not arg.startswith('--') and 'c' in arg:
                    if i+1 < len(args):
                        signal_check(args[i+1], depth+1)
                    break
        if base == 'eval':
            signal_check(' '.join(args[1:]), depth+1)

try:
    signal_check(cmd)
except Exception as error:
    _block(SIGNAL_REASON + ' Cannot safely parse this shell command: ' + str(error))
