import {
  defaultActionlintConfig,
  githubWorkflow,
  githubWorkflowEvent,
} from '../../repos/effect-utils/genie/external.ts'
import { linuxRunner, nixSetupSteps } from './shared.ts'

export default githubWorkflow({
  name: 'Nix',
  on: {
    pull_request: githubWorkflowEvent.all,
    push: { branches: ['main'] },
  },
  actionlint: defaultActionlintConfig,
  jobs: {
    build: {
      name: 'Nix build',
      'runs-on': linuxRunner,
      // Longer than the Node repository's 15: this build compiles Ghostty's
      // terminal core with Zig on a cold cache.
      'timeout-minutes': 40,
      steps: [
        { uses: 'actions/checkout@v4' },
        ...nixSetupSteps(),
        {
          name: 'Native binding contract and runtime closure',
          run: `nix build --no-link --print-build-logs \\
  .#checks.x86_64-linux.libghostty-contract \\
  .#checks.x86_64-linux.libghostty-runtime-closure`,
        },
        { run: 'nix build --print-build-logs' },
        { run: './result/bin/pty --version' },
      ],
    },
  },
})
