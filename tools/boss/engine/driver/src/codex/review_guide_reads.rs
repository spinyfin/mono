//! Shared Claude/Grok/Codex source inspection grammar, composed with the shell tokenizer.
pub(super) const SCRIPT: &str = r#"
def guide_object_path(path):
    return isinstance(path, str) and bool(path) and not path.startswith(('/', '-', ':')) and not any(
        part in ('', '.', '..', '.git', '.jj', '.codex', '.claude') or any(c in part for c in '*?[]\\\x00')
        for part in path.split('/'))

def guide_path(path, cwd):
    root = os.environ.get('BOSS_REVIEW_GUIDE_WORKSPACE', '')
    if not root or not isinstance(path, str) or not path or path.startswith('-') or '\x00' in path:
        return False
    root = os.path.realpath(root)
    candidate = os.path.realpath(os.path.join(cwd, path))
    try:
        if os.path.commonpath([root, candidate]) != root:
            return False
    except ValueError:
        return False
    relative = os.path.relpath(candidate, root)
    return not any(p in ('.jj', '.git', '.codex', '.claude') for p in relative.split(os.sep))

def guide_read_tool(payload):
    tool = payload.get('tool_name')
    data = payload.get('tool_input')
    if not isinstance(data, dict):
        return False
    cwd = payload.get('cwd') or os.environ.get('BOSS_REVIEW_GUIDE_WORKSPACE', '')
    if tool == 'Read':
        return guide_path(data.get('file_path'), cwd)
    if tool in ('Grep', 'Glob'):
        # Native tools have no program/preprocessor options; filesystem reads
        # remain scoped to the source root. Explicit metadata globbing is denied.
        pattern = data.get('glob', data.get('pattern', ''))
        if tool == 'Glob' and (os.path.isabs(pattern) or '..' in pattern.split('/')):
            return False
        if any(part in ('.git', '.jj', '.codex', '.claude') or (part.startswith('.') and any(c in part for c in '*?[')) for part in pattern.split('/')):
            return False
        return guide_path(data.get('path') or '.', cwd)
    return False

def guide_shell_read(payload):
    command = payload.get('tool_input', {}).get('command')
    if not isinstance(command, str):
        return False
    # No unquoted shell expansion, assignments, redirects, or operators.
    # Literals are masked before grouping so punctuation inside a search query
    # cannot be mistaken for shell syntax by the shared tokenizer.
    literal = r"'(?:[^']*)'"
    atom = '(?:[A-Za-z0-9_./:@+,-]+|\\||'+literal+')'
    if not re.fullmatch(atom+'(?:[ \t]+'+atom+')*[ \t]*', command):
        return False
    values = []
    def mask(match):
        values.append(match.group(0)[1:-1])
        return 'GUIDE_LITERAL_'+str(len(values)-1)
    masked = re.sub(literal, mask, command)
    # Pipelines joined only by '|'; every stage is itself a single allowed read.
    stages = masked.split('|')
    for index, stage in enumerate(stages):
        groups = command_groups(stage)
        if len(groups) != 1:
            return False
        tokens = [values[int(t[14:])] if t.startswith('GUIDE_LITERAL_') and t[14:].isdigit() and int(t[14:]) < len(values) else t for t in groups[0]]
        if not guide_read_stage(tokens, payload, index > 0):
            return False
    return True

def guide_read_stage(tokens, payload, piped):
    if not tokens:
        return False
    prog, args = tokens[0], tokens[1:]
    cwd = payload.get('cwd') or os.environ.get('BOSS_REVIEW_GUIDE_WORKSPACE', '')
    # A first stage must name its files; a piped stage reads stdin and names none.
    def files_ok(paths):
        if piped:
            return not paths
        return bool(paths) and all(guide_path(p, cwd) for p in paths)
    ranges = '[0-9]+(?:,[0-9]+)?p(?:;[0-9]+(?:,[0-9]+)?p)*'
    if prog == 'git':
        if piped:
            return False
        if args and args[0] == '--no-pager':
            args = args[1:]
        revisions = {os.environ.get('BOSS_REVIEW_GUIDE_HEAD_SHA'), os.environ.get('BOSS_REVIEW_GUIDE_BASE_SHA'), os.environ.get('BOSS_REVIEW_GUIDE_PREVIOUS_HEAD_SHA')}
        def pinned(value):
            return bool(re.fullmatch('[0-9a-fA-F]{40}', value)) and value in revisions
        if len(args) == 2 and args[0] == 'show':
            sha, sep, path = args[1].partition(':')
            return pinned(sha) and sep == ':' and guide_object_path(path)
        if len(args) >= 5 and args[:3] == ['diff', '--no-ext-diff', '--no-textconv']:
            rest = args[5:]
            return pinned(args[3]) and pinned(args[4]) and (not rest or (rest[0] == '--' and all(guide_object_path(p) for p in rest[1:])))
        return False
    if prog == 'cat':
        if args and args[0] == '-n':
            args = args[1:]
        return files_ok(args)
    if prog == 'nl':
        # Only explicit numbering options; never treat unknown options as paths.
        options = {'-b': '[atn]', '-h': '[atn]', '-f': '[atn]', '-n': '(?:ln|rn|rz)', '-i': '[0-9]+', '-v': '[0-9]+', '-w': '[0-9]+'}
        while args and args[0].startswith('-'):
            flag = args[0]
            args = args[1:]
            if flag == '-p':
                continue
            pattern = options.get(flag[:2])
            value = flag[2:]
            if not value and args:
                value, args = args[0], args[1:]
            if pattern is None or not re.fullmatch(pattern, value):
                return False
        return files_ok(args)
    if prog == 'wc':
        return len(args) >= 1 and args[0] == '-l' and files_ok(args[1:])
    if prog in ('head', 'tail'):
        if len(args) >= 2 and args[0] == '-n' and args[1].isdigit():
            args = args[2:]
        return files_ok(args)
    if prog == 'sed':
        return len(args) >= 2 and args[0] == '-n' and bool(re.fullmatch(ranges, args[1])) and files_ok(args[2:]) and (piped or len(args) == 3)
    if prog == 'rg':
        if piped:
            return False
        flags = {'-n', '--line-number', '-l', '--files-with-matches', '-i', '--ignore-case', '-F', '--fixed-strings', '--files'}
        files = '--files' in args
        while args and args[0] in flags:
            args = args[1:]
        if not files:
            if not args or args[0].startswith('-'):
                return False
            args = args[1:]
        return all(guide_path(p, cwd) for p in (args or ['.']))
    return False
"#;
