import {
  defaultActionlintConfig,
  githubWorkflow,
  githubWorkflowEvent,
} from '../../repos/effect-utils/genie/external.ts'
import { linuxRunner, nixSetupSteps } from './shared.ts'

export default githubWorkflow({
  name: 'Test',
  on: {
    pull_request: githubWorkflowEvent.all,
    push: { branches: ['main'] },
  },
  actionlint: defaultActionlintConfig,
  jobs: {
    test: {
      name: 'Test',
      'runs-on': linuxRunner,
      'timeout-minutes': 30,
      steps: [
        { uses: 'actions/checkout@v4' },
        ...nixSetupSteps(),

        // Reporting only. Neither passes on main today — 1093 fmt diffs and 50
        // clippy warnings, measured 2026-09-05 — so gating either one needs a
        // cleanup commit first, which is a separate decision from adding CI.
        // Printing the counts keeps the debt visible instead of hidden behind a
        // check nobody enabled.
        {
          name: 'Formatting and clippy (reporting only)',
          run: [
            `n=$(nix develop --command cargo fmt --all -- --check 2>/dev/null | grep -c '^Diff in' || true)`,
            'echo "cargo fmt --check: $n diff(s)" | tee -a "$GITHUB_STEP_SUMMARY"',
            'nix develop --command cargo clippy --workspace --all-targets 2>&1 | tee /tmp/clippy.log || true',
            `w=$(grep -c '^warning' /tmp/clippy.log || true)`,
            'echo "clippy: $w warning(s)" | tee -a "$GITHUB_STEP_SUMMARY"',
            '',
          ].join('\n'),
        },

        {
          name: 'Build',
          run: 'nix develop --command cargo build --workspace --release',
        },

        {
          name: 'Workspace tests',
          run: 'nix develop --command ./scripts/ci-test-workspace.sh',
        },
      ],
    },
  },
})
