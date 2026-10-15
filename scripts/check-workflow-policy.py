#!/usr/bin/env python3
"""Check generated pull-request cancellation policy without YAML dependencies.

Genie's schema and actionlint validate YAML; this checks only concurrency.
Run after `genie --check`.
"""

import re
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
GROUP = "${{ github.workflow }}-${{ github.event_name }}-${{ github.event.pull_request.number || github.run_id }}"
CANCEL = "${{ github.event_name == 'pull_request' }}"


def block(text, key):
    match = re.search(rf"^{re.escape(key)}:\s*\n((?:(?: [^\n]*|)\n)*)", text, re.M)
    return match.group(1) if match else ""


def scalar(value):
    return value.strip().strip('"')


def cancellation_errors(text, is_pr):
    errors = []
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
        self.assertEqual(len(paths), 6, 'audit new workflows for cancellation policy')
        for path in paths:
            with self.subTest(workflow=path.name):
                self.assertEqual(cancellation_errors(path.read_text(), path.name != 'release.yml'), [])

    def test_reject_missing_or_broad_cancellation(self):
        text = (ROOT / '.github/workflows/test.yml').read_text()
        self.assertTrue(cancellation_errors(text.replace(CANCEL, 'true'), True))
        self.assertTrue(cancellation_errors(text.replace(GROUP, '${{ github.workflow }}-${{ github.ref }}'), True))
        self.assertTrue(cancellation_errors(text.replace('concurrency:', 'missing-concurrency:'), True))
        self.assertTrue(cancellation_errors(text, False))

    def test_concurrency_identity_scenarios(self):
        text = (ROOT / '.github/workflows/test.yml').read_text()
        values = dict(re.findall(r'^  ([\w-]+): (.+)$', block(text, 'concurrency'), re.M))
        template = scalar(values['group'])

        def group(workflow, event, pr, run):
            context = {'github.workflow': workflow, 'github.event_name': event,
                       'github.event.pull_request.number': pr, 'github.run_id': run}
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


if __name__ == '__main__':
    unittest.main()
