import {
  defaultActionlintConfig,
  githubWorkflow,
  githubWorkflowEvent,
} from '../../repos/effect-utils/genie/external.ts'
import { linuxRunner } from './shared.ts'

export default githubWorkflow({
  name: 'Private names',
  on: {
    pull_request: githubWorkflowEvent.all,
    push: { branches: ['main'] },
  },
  actionlint: defaultActionlintConfig,
  jobs: {
    check: {
      name: 'Private names',
      'runs-on': linuxRunner,
      'timeout-minutes': 5,
      steps: [
        { uses: 'actions/checkout@v4' },
        {
          name: 'The check catches what it should',
          run: 'python3 scripts/check-private-names.py --self-test',
        },
        {
          name: 'No private machine, person, or agent names',
          run: 'python3 scripts/check-private-names.py',
        },
      ],
    },
  },
})
