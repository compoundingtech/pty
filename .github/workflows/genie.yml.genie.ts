import {
  githubWorkflow,
  githubWorkflowEvent,
  nixDevelopStep,
  plainFlakeJob,
} from '../../repos/effect-utils/genie/external.ts'
import { defaultActionlintConfig, pullRequestConcurrency, linuxRunner, nixSetupOptions } from './shared.ts'

export default githubWorkflow({
  concurrency: pullRequestConcurrency,
  name: 'Genie',
  on: {
    pull_request: githubWorkflowEvent.all,
    push: { branches: ['main'] },
  },
  actionlint: defaultActionlintConfig,
  jobs: {
    // The generator shell is separate from the default Rust shell, so only this
    // job builds or substitutes genie.
    freshness: plainFlakeJob({
      name: 'genie freshness',
      runsOn: linuxRunner,
      nix: nixSetupOptions,
      'timeout-minutes': 15,
      step: nixDevelopStep({
        name: 'Check generated files',
        flake: '.#genie',
        command: ['sh', '-c', 'genie --check && python3 scripts/check-workflow-policy.py'],
      }),
    }),
  },
})
