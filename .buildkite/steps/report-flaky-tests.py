#!/usr/bin/env python3
"""Publish Bazel FLAKY summaries and their individual attempt outputs."""

import hashlib
import html
import json
import os
from pathlib import Path
import re
import shutil
import subprocess
import sys
from urllib.parse import unquote, urlparse
import xml.etree.ElementTree as ET


def identity(event_id):
    return event_id['label'], event_id.get('configuration', {}).get('id', '')


def failed_cases(path):
    try:
        root = ET.parse(path).getroot()
    except (ET.ParseError, OSError):
        return []
    return sorted({'.'.join(filter(None, [case.get('classname'), case.get('name')]))
                   for case in root.iter('testcase')
                   if case.find('failure') is not None or case.find('error') is not None})


# libtest names each failing test on a line of its own. Bazel writes the
# test.xml for a test binary that produces none, and that file has a single
# case named after the wrapper script, so for a Rust target the log is the only
# place the failing test is named.
LIBTEST_FAILURE = re.compile(r'^test (\S+) \.\.\. FAILED\s*$', re.MULTILINE)


def failed_cases_in_log(path):
    try:
        return set(LIBTEST_FAILURE.findall(Path(path).read_text(errors='replace')))
    except OSError:
        return set()


def copy_outputs(items, output, folder, cases, seen, required=False, log_cases=None):
    paths, missing = [], []
    for index, item in enumerate(items):
        uri = item.get('uri', '')
        if uri in seen:
            continue
        parsed = urlparse(uri)
        if parsed.scheme != 'file':
            raise ValueError(f'Expected a local BEP output, got {uri}')
        source = Path(unquote(parsed.path))
        name = Path(item.get('name') or source.name).name
        if not required and not source.is_file() and name not in ('test.log', 'test.xml'):
            missing.append(uri)
            continue
        destination = folder / f'{index}-{name}'
        (output / folder).mkdir(parents=True, exist_ok=True)
        shutil.copyfile(source, output / destination)
        paths.append(destination.as_posix())
        seen.add(uri)
        if name == 'test.xml':
            cases.update(failed_cases(source))
        elif name == 'test.log' and log_cases is not None:
            log_cases.update(failed_cases_in_log(source))
    return paths, missing


def collect(bep, output):
    results, summaries = {}, {}
    with bep.open() as stream:
        for line in stream:
            event = json.loads(line)
            if 'testResult' in event:
                key = identity(event['id']['testResult'])
                results.setdefault(key, []).append(event)
            if event.get('testSummary', {}).get('overallStatus') == 'FLAKY':
                summaries[identity(event['id']['testSummary'])] = event['testSummary']
    tests = []
    for key, summary in sorted(summaries.items()):
        attempts, cases, log_cases, seen = [], set(), set(), set()
        base = Path('flaky-logs') / hashlib.sha256(repr(key).encode()).hexdigest()[:16]
        for event in results.get(key, []):
            test_id = event['id']['testResult']
            attempt = {field: test_id.get(field, 1) for field in ('run', 'shard', 'attempt')}
            result = event['testResult']
            cached = result.get('cachedLocally', False)
            attempt.update(status=result['status'], cached_locally=cached,
                           provenance='local-cache' if cached else 'test-result',
                           execution_info=result.get('executionInfo', {}))
            folder = (base
                      / f"run-{attempt['run']}-shard-{attempt['shard']}-attempt-{attempt['attempt']}")
            # Only a failed attempt's log names what failed: a passing retry
            # prints the same test with `ok`.
            paths, missing = copy_outputs(result.get('testActionOutput', []),
                                          output, folder, cases, set(),
                                          log_cases=log_cases if result['status'] == 'FAILED' else None)
            seen.update(item.get('uri') for item in result.get('testActionOutput', []))
            attempt.update(log_paths=paths, missing_outputs=missing)
            attempts.append(attempt)
        summary_logs = []
        for status, field in (('FAILED', 'failed'), ('PASSED', 'passed')):
            paths, missing = copy_outputs(summary.get(field, []), output,
                                          base / ('summary-' + field), cases, seen, required=True,
                                          log_cases=log_cases if status == 'FAILED' else None)
            if paths or missing:
                summary_logs.append(dict(status=status, log_paths=paths, missing_outputs=missing))
        if not attempts:
            raise ValueError(f'FLAKY summary has no attempt results: {key[0]}')
        tests.append(dict(target=key[0], configuration=key[1],
                          attempt_count=summary.get('totalRunCount', len(attempts)),
                          test_cases=sorted(log_cases or cases), attempts=attempts,
                          replayed=all(a['cached_locally'] for a in attempts),
                          summary_logs=summary_logs))
    return dict(commit=os.environ.get('BUILDKITE_COMMIT', ''),
                branch=os.environ.get('BUILDKITE_BRANCH', ''),
                build_number=os.environ.get('BUILDKITE_BUILD_NUMBER', ''), tests=tests)


def step_log_summary(report):
    """The lines that put a flaky run in the step's own log.

    Bazel counts a test that passed on its retry as passed, so the step ends
    green and its last lines say only that every test passed. `+++` opens the
    group in the Buildkite log, where the default is collapsed.
    """
    count = len(report['tests'])
    lines = [f"+++ [bazel-test] {count} flaky test target{'' if count == 1 else 's'}: "
             'failed, then passed on the retry']
    for test in report['tests']:
        lines.append(f"  {test['target']} ({test['attempt_count']} attempts)")
        lines.extend(f'    {case}' for case in test['test_cases'])
    lines.append('  The step passes because of the retry (--flaky_test_attempts). Each '
                 "attempt's log is in the flaky-logs/ artifacts and linked from the build's annotation.")
    return lines


def publish(report, output):
    (output / 'flaky-tests.json').write_text(json.dumps(report, indent=2) + '\n')
    subprocess.run(['buildkite-agent', 'artifact', 'upload', 'flaky-tests.json'],
                   cwd=output, check=True)
    if not report['tests']:
        return
    print('\n'.join(step_log_summary(report)), flush=True)
    if (output / 'flaky-logs').exists():
        subprocess.run(['buildkite-agent', 'artifact', 'upload', 'flaky-logs/**/*'],
                       cwd=output, check=True)
    lines = ['### Flaky tests (passed after retry)', '']
    for test in report['tests']:
        lines.append(f"- <code>{html.escape(test['target'])}</code> ({test['attempt_count']} attempts)")
        for case in test['test_cases']:
            lines.append(f"  - Test case: <code>{html.escape(case)}</code>")
        for attempt in test['attempts']:
            label = (f"run {attempt['run']}, shard {attempt['shard']}, "
                     f"attempt {attempt['attempt']}: {attempt['status']}")
            if attempt['cached_locally']:
                label = f"local cache replay: aggregate {attempt['status']} (original attempts unavailable)"
            lines.append(f'  - {label}')
            for path in attempt['log_paths']:
                link = 'artifact://' + path
                lines.append(f'    - <a href="{html.escape(link, quote=True)}">{html.escape(path)}</a>')
        for entry in test['summary_logs']:
            for path in entry['log_paths']:
                link = html.escape('artifact://' + path, quote=True)
                lines.append(f"  - Summary {entry['status']}: <a href=\"{link}\">{html.escape(path)}</a>")
    subprocess.run(['buildkite-agent', 'annotate', '--style', 'warning', '--context',
                    'bazel-flaky-' + os.environ.get('BUILDKITE_JOB_ID', 'step'),
                    '\n'.join(lines)], check=True)


if __name__ == '__main__':
    publish(collect(Path(sys.argv[1]), Path(sys.argv[2])), Path(sys.argv[2]))
