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
        attempts, cases = [], set()
        for event in results.get(key, []):
            test_id = event['id']['testResult']
            attempt = {field: test_id.get(field, 1) for field in ('run', 'shard', 'attempt')}
            attempt.update(status=event['testResult']['status'], log_paths=[])
            folder = (Path('flaky-logs') / hashlib.sha256(repr(key).encode()).hexdigest()[:16]
                      / f"run-{attempt['run']}-shard-{attempt['shard']}-attempt-{attempt['attempt']}")
            for index, item in enumerate(event['testResult'].get('testActionOutput', [])):
                uri = item.get('uri', '')
                parsed = urlparse(uri)
                if parsed.scheme == 'file':
                    source = Path(unquote(parsed.path))
                    destination = folder / f"{index}-{Path(item['name']).name}"
                    (output / folder).mkdir(parents=True, exist_ok=True)
                    shutil.copyfile(source, output / destination)
                    attempt['log_paths'].append(destination.as_posix())
                    if item['name'] == 'test.xml':
                        cases.update(failed_cases(source))
                elif uri:
                    raise ValueError(f'Expected a local BEP output, got {uri}')
            attempts.append(attempt)
        if not attempts:
            raise ValueError(f'FLAKY summary has no attempt results: {key[0]}')
        tests.append(dict(target=key[0], configuration=key[1],
                          attempt_count=summary.get('totalRunCount', len(attempts)),
                          test_cases=sorted(cases), attempts=attempts))
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
            lines.append(f'  - {label}')
            for path in attempt['log_paths']:
                link = 'artifact://' + path
                lines.append(f'    - <a href="{html.escape(link, quote=True)}">{html.escape(path)}</a>')
    subprocess.run(['buildkite-agent', 'annotate', '--style', 'warning', '--context',
                    'bazel-flaky-' + os.environ.get('BUILDKITE_JOB_ID', 'step'),
                    '\n'.join(lines)], check=True)


if __name__ == '__main__':
    publish(collect(Path(sys.argv[1]), Path(sys.argv[2])), Path(sys.argv[2]))
