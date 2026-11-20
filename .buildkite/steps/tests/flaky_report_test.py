"""Exercise BEP parsing and artifact/annotation publication without an agent."""
import importlib.util
import json
import os
from pathlib import Path
import subprocess
import tempfile
import unittest
from unittest.mock import patch

spec = importlib.util.spec_from_file_location(
    'report', Path(__file__).resolve().parents[1] / 'report-flaky-tests.py')
reporter = importlib.util.module_from_spec(spec)
spec.loader.exec_module(reporter)


class ReportTest(unittest.TestCase):
    def test_flaky_attempts_and_publication(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            log = root / 'attempt log.txt'
            log.write_text('failed once')
            xml = root / 'test.xml'
            xml.write_text('<testsuite><testcase classname="suite" name="case">'
                           '<failure/></testcase></testsuite>')
            events = []
            for shard in (1, 2):
                for attempt, status in ((1, 'FAILED'), (2, 'PASSED')):
                    events.append(dict(
                        id=dict(testResult=dict(label='//pkg:flaky',
                                                configuration=dict(id='cfg'),
                                                shard=shard, attempt=attempt)),
                        testResult=dict(status=status, testActionOutput=[
                            dict(name='test.log', uri=log.as_uri()),
                            dict(name='test.xml', uri=xml.as_uri())])))
            for target, status in (('flaky', 'FLAKY'), ('red', 'FAILED'), ('green', 'PASSED')):
                events.append(dict(id=dict(testSummary=dict(label='//pkg:' + target,
                                                            configuration=dict(id='cfg'))),
                                   testSummary=dict(overallStatus=status, totalRunCount=4)))
            bep = root / 'events.json'
            bep.write_text('\n'.join(map(json.dumps, events)))
            with patch.dict(os.environ, BUILDKITE_COMMIT='abc', BUILDKITE_BRANCH='main',
                            BUILDKITE_BUILD_NUMBER='123'):
                report = reporter.collect(bep, root)
            self.assertEqual(report['commit'], 'abc')
            self.assertEqual(report['branch'], 'main')
            self.assertEqual(report['build_number'], '123')
            self.assertEqual(len(report['tests']), 1)
            test = report['tests'][0]
            self.assertEqual(test['target'], '//pkg:flaky')
            self.assertEqual(test['attempt_count'], 4)
            self.assertEqual(test['test_cases'], ['suite.case'])
            self.assertEqual(len(test['attempts']), 4)
            paths = [path for attempt in test['attempts'] for path in attempt['log_paths']]
            self.assertEqual(len(set(paths)), 8)
            for path in paths:
                self.assertTrue((root / path).is_file())
            with patch.object(reporter.subprocess, 'run') as run:
                reporter.publish(report, root)
            self.assertEqual(json.loads((root / 'flaky-tests.json').read_text()), report)
            self.assertEqual(run.call_count, 3)
            annotation = run.call_args.args[0][-1]
            self.assertIn('//pkg:flaky', annotation)
            self.assertIn('suite.case', annotation)
            for path in paths:
                self.assertIn('artifact://' + path, annotation)

    def test_empty_report_uploads_artifact_without_warning(self):
        with tempfile.TemporaryDirectory() as temp:
            with patch.object(reporter.subprocess, 'run') as run:
                reporter.publish(dict(tests=[]), Path(temp))
            self.assertEqual(run.call_count, 1)
            self.assertIn('flaky-tests.json', run.call_args.args[0])

    def test_invalid_bep_fails_loudly(self):
        with tempfile.TemporaryDirectory() as temp:
            bep = Path(temp) / 'events.json'
            bep.write_text('{')
            with self.assertRaises(json.JSONDecodeError):
                reporter.collect(bep, Path(temp))

    def test_agent_failure_is_not_suppressed(self):
        with tempfile.TemporaryDirectory() as temp:
            with patch.object(reporter.subprocess, 'run', side_effect=OSError('agent unavailable')):
                with self.assertRaises(OSError):
                    reporter.publish(dict(tests=[]), Path(temp))

    def test_missing_attempt_evidence_fails_loudly(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            bep = root / 'events.json'
            summary = dict(id=dict(testSummary=dict(label='//pkg:flaky')),
                           testSummary=dict(overallStatus='FLAKY'))
            bep.write_text(json.dumps(summary))
            with self.assertRaises(ValueError):
                reporter.collect(bep, root)
            result = dict(id=dict(testResult=dict(label='//pkg:flaky')),
                          testResult=dict(status='FAILED', testActionOutput=[
                              dict(name='test.log', uri=(root / 'missing.log').as_uri())]))
            bep.write_text(json.dumps(result) + '\n' + json.dumps(summary))
            with self.assertRaises(FileNotFoundError):
                reporter.collect(bep, root)

    def test_step_status_and_ci_retry_config(self):
        root = Path(os.environ['TEST_SRCDIR']) / os.environ['TEST_WORKSPACE']
        self.assertIn('test:ci --flaky_test_attempts=2', (root / '.ci.bazelrc').read_text())
        original = (root / '.buildkite/steps/bazel-build-test.sh').read_text()
        # Execute the real control flow with stand-ins for Bazel and the agent.
        # This covers the report running after a failed test and failing closed.
        with tempfile.TemporaryDirectory() as temp:
            folder = Path(temp)
            script = folder / 'step.sh'
            script.write_text(original)
            (folder / 'ci-env.sh').write_text('''bazel_verify_only() {
  if [[ "$1" == "test" ]]; then return "$TEST_STATUS"; fi
}
''')
            (folder / 'report-flaky-tests.py').write_text('''import os, pathlib, sys
pathlib.Path(os.environ['REPORT_MARKER']).touch()
sys.exit(int(os.environ['REPORT_STATUS']))
''')
            marker = folder / 'reported'
            for test_status, report_status, expected in ((0, 0, 0), (1, 0, 1), (0, 1, 1), (1, 1, 1)):
                marker.unlink(missing_ok=True)
                env = dict(os.environ, TEST_STATUS=str(test_status),
                           REPORT_STATUS=str(report_status), REPORT_MARKER=str(marker))
                result = subprocess.run(['bash', str(script)], env=env, capture_output=True)
                self.assertEqual(result.returncode, expected, result.stderr)
                self.assertTrue(marker.exists())


if __name__ == '__main__':
    unittest.main()
