#!/usr/bin/env python3
"""Check generated CI scheduling controls without installing YAML dependencies.

Genie's schema and actionlint validate YAML; this check inspects only the emitted
runner and concurrency blocks. Run after `genie --check`.
"""

import re
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
GROUP = "${{ github.workflow }}-${{ github.event_name }}-${{ github.event.pull_request.number || github.run_id }}"
CANCEL = "${{ github.event_name == 'pull_request' }}"


def block(text, key, indent=0):
    match = re.search(rf"^{' ' * indent}{re.escape(key)}:\s*\n((?:(?:{' ' * (indent + 1)}[^\n]*|)\n)*)", text, re.M)
    return match.group(1) if match else ""


def scalar(value):
    return value.strip().strip('"')


def scheduling_errors(text, is_pr):
    errors = []
    jobs = block(text, 'jobs')
    for match in re.finditer(r'^  ([\w-]+):\n', jobs, re.M):
        job = block(jobs, match.group(1), 2)
        runner = block(job, 'runs-on', 4)
        # Genie can render a short label array on the same line as runs-on.
        inline = re.search(r'^    runs-on: (.+)$', job, re.M)
        runner += inline.group(1) if inline else ''
        if 'namespace-profile-' in runner:
            if runner.count('job.priority=2') != 1:
                errors.append(f'{match.group(1)}: expected one delivery priority 2')
            if 'github.run-id=${{ github.run_id }}' not in runner:
                errors.append(f'{match.group(1)}: missing existing run affinity')
    concurrency = block(text, 'concurrency')
    if is_pr:
        values = dict(re.findall(r'^  ([\w-]+): (.+)$', concurrency, re.M))
        if scalar(values.get('group', '')) != GROUP:
            errors.append('PR group must distinguish workflows and PRs, and isolate non-PR runs')
        if scalar(values.get('cancel-in-progress', '')) != CANCEL:
            errors.append('only pull requests may cancel earlier runs')
    elif concurrency:
        errors.append('release runs must remain independent')
    return errors


class WorkflowPolicy(unittest.TestCase):
    def test_generated_workflows(self):
        paths = list((ROOT / '.github/workflows').glob('*.yml'))
        self.assertEqual(len(paths), 6, 'audit new workflows for scheduling policy')
        for path in paths:
            with self.subTest(workflow=path.name):
                text = path.read_text()
                self.assertEqual(scheduling_errors(text, path.name != 'release.yml'), [])
                expected = 2 if path.name in {'release.yml', 'conformance.yml'} else 1
                self.assertEqual(text.count('job.priority=2'), expected)

    def test_controls_reject_missing_priority_and_broad_cancellation(self):
        text = (ROOT / '.github/workflows/test.yml').read_text()
        self.assertTrue(scheduling_errors(text.replace('job.priority=2', 'job.priority=5'), True))
        self.assertTrue(scheduling_errors(text.replace(CANCEL, 'true'), True))
        self.assertTrue(scheduling_errors(text.replace(GROUP, '${{ github.workflow }}-${{ github.ref }}'), True))

    def test_concurrency_identity_scenarios(self):
        text = (ROOT / '.github/workflows/test.yml').read_text()
        values = dict(re.findall(r'^  ([\w-]+): (.+)$', block(text, 'concurrency'), re.M))
        template = scalar(values['group'])

        def group(workflow, event, pr, run):
            context = {'github.workflow': workflow, 'github.event_name': event,
                       'github.event.pull_request.number': pr, 'github.run_id': run}
            # Resolve the emitted template's property lookups and fallback operator.
            return re.sub(r'\$\{\{ (.*?) \}\}', lambda match: str(next(
                context[key.strip()] for key in match.group(1).split('||')
                if context.get(key.strip())
            )), template)

        self.assertEqual(group('Test', 'pull_request', 12, 100), group('Test', 'pull_request', 12, 101))
        self.assertNotEqual(group('Test', 'pull_request', 12, 100), group('Test', 'pull_request', 13, 101))
        self.assertNotEqual(group('Test', 'pull_request', 12, 100), group('Nix', 'pull_request', 12, 100))
        for event in ['push', 'workflow_dispatch']:
            with self.subTest(event=event):
                self.assertNotEqual(group('Test', event, None, 100), group('Test', event, None, 101))

    def test_both_release_platforms_are_prioritized(self):
        text = (ROOT / '.github/workflows/release.yml').read_text()
        self.assertIn('namespace-profile-linux-x86-64', text)
        self.assertIn('namespace-profile-macos-arm64', text)
        self.assertEqual(text.count('job.priority=2'), 2)
        self.assertIn('runs-on: ubuntu-latest', text)


if __name__ == '__main__':
    unittest.main()
