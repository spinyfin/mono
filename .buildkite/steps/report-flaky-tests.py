#!/usr/bin/env python3
"""Publish Bazel FLAKY summaries and their individual attempt outputs."""

import hashlib
import html
import json
import os
from pathlib import Path
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


def copy_outputs(items, output, folder, cases, seen, required=False):
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
        attempts, cases, seen = [], set(), set()
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
            paths, missing = copy_outputs(result.get('testActionOutput', []),
                                          output, folder, cases, set())
            seen.update(item.get('uri') for item in result.get('testActionOutput', []))
            attempt.update(log_paths=paths, missing_outputs=missing)
            attempts.append(attempt)
        summary_logs = []
        for status, field in (('FAILED', 'failed'), ('PASSED', 'passed')):
            paths, missing = copy_outputs(summary.get(field, []), output,
                                          base / ('summary-' + field), cases, seen, required=True)
            if paths or missing:
                summary_logs.append(dict(status=status, log_paths=paths, missing_outputs=missing))
        if not attempts:
            raise ValueError(f'FLAKY summary has no attempt results: {key[0]}')
        tests.append(dict(target=key[0], configuration=key[1],
                          attempt_count=summary.get('totalRunCount', len(attempts)),
                          test_cases=sorted(cases), attempts=attempts,
                          replayed=all(a['cached_locally'] for a in attempts),
                          summary_logs=summary_logs))
    return dict(commit=os.environ.get('BUILDKITE_COMMIT', ''),
                branch=os.environ.get('BUILDKITE_BRANCH', ''),
                build_number=os.environ.get('BUILDKITE_BUILD_NUMBER', ''), tests=tests)


def publish(report, output):
    (output / 'flaky-tests.json').write_text(json.dumps(report, indent=2) + '\n')
    subprocess.run(['buildkite-agent', 'artifact', 'upload', 'flaky-tests.json'],
                   cwd=output, check=True)
    if not report['tests']:
        return
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
