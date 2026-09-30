import {
  githubRepoSettings,
  githubRuleset,
} from '../repos/effect-utils/genie/external.ts'

// Admin rollout only: apply after these Namespace checks have passed once.
export default githubRepoSettings({
  repository: {
    allow_auto_merge: true,
    delete_branch_on_merge: true,
  },
  rulesets: [
    githubRuleset({
      name: 'main',
      target: 'branch',
      enforcement: 'active',
      conditions: {
        ref_name: { include: ['refs/heads/main'], exclude: [] },
      },
      bypass_actors: [],
      rules: [
        {
          type: 'required_status_checks',
          parameters: {
            strict_required_status_checks_policy: true,
            required_status_checks: [
              'Nix build',
              'Private names',
              'Test',
              'Conformance gate',
              'genie freshness',
            ].map((context) => ({ context })),
          },
        },
      ],
    }),
  ],
})
