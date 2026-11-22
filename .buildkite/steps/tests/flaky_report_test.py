"""Exercise BEP parsing and artifact/annotation publication without an agent."""
from contextlib import redirect_stdout
import importlib.util
import io
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

    def test_rust_failure_is_named_from_the_failed_attempt_log(self):
        # A Rust test binary writes no XML: Bazel's stand-in names the wrapper
        # script, and only the failed attempt's log names the test that failed.
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            xml = root / 'test.xml'
            xml.write_text('<testsuite><testcase name="pkg/flaky_test_wrapper.sh">'
                           '<error message="exited with error code 101"/></testcase></testsuite>')
            failed, passed = root / 'failed.log', root / 'passed.log'
            failed.write_text('test other::case ... ok\n'
                              'test suite::hangs has been running for over 60 seconds\n'
                              'test suite::hangs ... FAILED\n\nfailures:\n    suite::hangs\n')
            passed.write_text('test suite::hangs ... ok\ntest other::case ... ok\n')
            events = [dict(id=dict(testResult=dict(label='//pkg:flaky', attempt=attempt)),
                           testResult=dict(status=status, testActionOutput=[
                               dict(name='test.log', uri=log.as_uri()),
                               dict(name='test.xml', uri=xml.as_uri())]))
                      for attempt, status, log in ((1, 'FAILED', failed), (2, 'PASSED', passed))]
            events.append(dict(id=dict(testSummary=dict(label='//pkg:flaky')),
                               testSummary=dict(overallStatus='FLAKY', totalRunCount=2)))
            bep = root / 'events.json'
            bep.write_text('\n'.join(map(json.dumps, events)))
            report = reporter.collect(bep, root)
            self.assertEqual(report['tests'][0]['test_cases'], ['suite::hangs'])
            printed = io.StringIO()
            with patch.object(reporter.subprocess, 'run') as run, redirect_stdout(printed):
                reporter.publish(report, root)
            # The annotation and the step's own log both name the test.
            self.assertIn('suite::hangs', run.call_args.args[0][-1])
            self.assertNotIn('flaky_test_wrapper.sh', run.call_args.args[0][-1])
            lines = printed.getvalue().splitlines()
            self.assertEqual(lines[0], '+++ [bazel-test] 1 flaky test target: failed, then passed on the retry')
            self.assertIn('  //pkg:flaky (2 attempts)', lines)
            self.assertIn('    suite::hangs', lines)

    def test_empty_report_uploads_artifact_without_warning(self):
        with tempfile.TemporaryDirectory() as temp:
            printed = io.StringIO()
            with patch.object(reporter.subprocess, 'run') as run, redirect_stdout(printed):
                reporter.publish(dict(tests=[]), Path(temp))
            self.assertEqual(run.call_count, 1)
            self.assertIn('flaky-tests.json', run.call_args.args[0])
            self.assertEqual(printed.getvalue(), '')

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

    def test_missing_optional_remote_outputs_are_recorded(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            log = root / 'test.log'
            log.write_text('recovered')
            outputs = [dict(name='test.log', uri=log.as_uri()),
                       dict(name='test.outputs__outputs.zip', uri=(root / 'outputs.zip').as_uri())]
            result = dict(id=dict(testResult=dict(label='//pkg:remote')),
                          testResult=dict(status='PASSED', testActionOutput=outputs))
            summary = dict(id=dict(testSummary=dict(label='//pkg:remote')),
                           testSummary=dict(overallStatus='FLAKY'))
            bep = root / 'events.json'
            bep.write_text('\n'.join(map(json.dumps, [result, summary])))
            report = reporter.collect(bep, root)
            attempt = report['tests'][0]['attempts'][0]
            self.assertFalse(report['tests'][0]['replayed'])
            self.assertEqual(attempt['missing_outputs'], [outputs[1]['uri']])
            self.assertEqual((root / attempt['log_paths'][0]).read_text(), 'recovered')
            with patch.object(reporter.subprocess, 'run'):
                reporter.publish(report, root)
            outputs.append(dict(name='test.xml', uri=(root / 'test.xml').as_uri()))
            bep.write_text('\n'.join(map(json.dumps, [result, summary])))
            with self.assertRaises(FileNotFoundError):
                reporter.collect(bep, root)

    def test_cached_replay_and_missing_optional_output(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            passed, failed = root / 'test.log', root / 'failed.log'
            passed.write_text('passed on retry')
            failed.write_text('original failure')
            events = [dict(id=dict(testResult=dict(label='//pkg:cached')),
                           testResult=dict(status='FLAKY', cachedLocally=True, testActionOutput=[
                               dict(name='test.log', uri=passed.as_uri()),
                               dict(name='outputs.zip', uri=(root / 'absent.zip').as_uri())])),
                      dict(id=dict(testSummary=dict(label='//pkg:cached')),
                           testSummary=dict(overallStatus='FLAKY', totalRunCount=2,
                                            failed=[dict(uri=failed.as_uri())],
                                            passed=[dict(uri=passed.as_uri())]))]
            bep = root / 'events.json'
            bep.write_text('\n'.join(map(json.dumps, events)))
            report = reporter.collect(bep, root)
            test = report['tests'][0]
            self.assertTrue(test['replayed'])
            self.assertEqual(test['attempts'][0]['provenance'], 'local-cache')
            self.assertEqual(test['attempts'][0]['missing_outputs'], [(root / 'absent.zip').as_uri()])
            self.assertEqual(len(test['summary_logs']), 1)
            path = test['summary_logs'][0]['log_paths'][0]
            self.assertEqual((root / path).read_text(), 'original failure')
            with patch.object(reporter.subprocess, 'run') as run:
                reporter.publish(report, root)
            annotation = run.call_args.args[0][-1]
            self.assertIn('local cache replay: aggregate FLAKY', annotation)
            self.assertNotIn('attempt 1: FLAKY', annotation)
            self.assertIn('artifact://' + path, annotation)

    def test_step_status_and_ci_retry_config(self):
        root = Path(os.environ['TEST_SRCDIR']) / os.environ['TEST_WORKSPACE']
        self.assertNotIn('--flaky_test_attempts', (root / '.ci.bazelrc').read_text())
        original = (root / '.buildkite/steps/bazel-build-test.sh').read_text()
        self.assertIn('test --flaky_test_attempts=2', original)
        self.assertIn("--remote_download_regex='.*testlogs/.*\\.(log|xml)$'", original)
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
